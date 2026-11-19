package io.github.jotrorox.rift.messaging;

import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Collection;
import java.util.List;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadLocalRandom;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Consumer;

/**
 * A client for Rift's messaging bus over QUIC ({@code docs/messaging-protocol.md}): TLS 1.3 with
 * explicitly trusted certificates, ALPN {@code rift-messaging/1}, token authentication,
 * publish, request/reply, subscriptions and retained streams.
 *
 * <p>The client connects in the background and reconnects with exponential backoff after a
 * failure; subscriptions are re-established on every new connection before listeners hear
 * {@link ConnectionListener#connected()}. Publications and requests fail fast while disconnected
 * and are never retried automatically: a failed publication may still have reached the broker.
 *
 * <p>Give every peer of a Rift cluster with {@link Builder#addAddress(String, int)}: the client
 * connects to the first that accepts, and after losing a connection it moves on to the next address
 * at once, pausing with growing delays only after every address failed once.
 *
 * <pre>{@code
 * MessagingClient bus = MessagingClient.builder()
 *         .address("127.0.0.1", 4222).serverName("localhost")
 *         .trust(Path.of("messaging-cert.pem"))
 *         .token(System.getenv("RIFT_PAPER_TOKEN"))
 *         .build();
 * bus.connected().join();
 * bus.subscribe("game.player.*", null, message -> System.out.println(message.text())).join();
 * bus.publish("game.player.join", "hello".getBytes(StandardCharsets.UTF_8)).join();
 * }</pre>
 */
public final class MessagingClient implements AutoCloseable {
    /** How long to wait before re-establishing a subscription the server ended. */
    private static final Duration RESUBSCRIBE_DELAY = Duration.ofSeconds(1);

    /** One address of the bus and the name its certificate is verified for. */
    record Address(String host, int port, String serverName) {}

    record Settings(
            List<Address> addresses,
            KeyStore trust,
            String token,
            Duration connectTimeout,
            Duration minBackoff,
            Duration maxBackoff) {}

    private final Settings settings;
    private final ExecutorService readers = Executors.newCachedThreadPool(daemon("rift-messaging-reader"));
    private final ScheduledExecutorService timers =
            Executors.newSingleThreadScheduledExecutor(daemon("rift-messaging-timer"));
    private final List<ConnectionListener> listeners = new CopyOnWriteArrayList<>();
    private final Set<Managed> subscriptions = ConcurrentHashMap.newKeySet();
    private final Thread connector;
    private volatile Session session;
    private volatile CompletableFuture<Void> connected = new CompletableFuture<>();
    private volatile boolean closed;

    private MessagingClient(Settings settings) {
        this.settings = settings;
        this.connector = daemon("rift-messaging-connector").newThread(this::run);
    }

    /** @return a builder */
    public static Builder builder() {
        return new Builder();
    }

    /** @return completes when the client is connected now, or next connects */
    public CompletableFuture<Void> connected() {
        return connected;
    }

    /** @return whether a connection is established */
    public boolean isConnected() {
        return session != null;
    }

    /** @param listener called on connection changes, starting with the next one */
    public void addListener(ConnectionListener listener) {
        listeners.add(Objects.requireNonNull(listener, "listener"));
    }

    /** @param listener a listener to remove */
    public void removeListener(ConnectionListener listener) {
        listeners.remove(listener);
    }

    /**
     * Publish and wait for the broker's acceptance (PUBLISH_ACK on a shared ordered stream).
     * Acceptance means live routing, not that a subscriber processed the message.
     *
     * @param subject a literal subject
     * @param payload at most 1 MiB
     * @return completes when the broker accepted the message
     */
    public CompletableFuture<Void> publish(String subject, byte[] payload) {
        return publish(subject, null, payload);
    }

    /**
     * @param subject a literal subject
     * @param reply an {@code _INBOX.} subject covered by this identity's subscribe grants, or null
     * @param payload at most 1 MiB
     * @return completes when the broker accepted the message
     */
    public CompletableFuture<Void> publish(String subject, String reply, byte[] payload) {
        Session current = session;
        return current == null ? notConnected() : current.publish(subject, reply, payload);
    }

    /**
     * Send a request to one responder and wait for its reply. The broker creates a private inbox, so
     * the identity needs only publish permission on {@code subject}.
     *
     * @param subject a literal subject
     * @param payload at most 1 MiB
     * @param timeout 1 ms to 120 s
     * @return the reply; fails with {@link java.util.concurrent.TimeoutException} when none arrives
     *     in time, or {@link MessagingException} when the broker refuses (for example
     *     {@code no responders available})
     */
    public CompletableFuture<Message> request(String subject, byte[] payload, Duration timeout) {
        Session current = session;
        return current == null ? notConnected() : current.request(subject, payload, timeout);
    }

    /**
     * Subscribe to a pattern ({@code *} one token, trailing {@code >} the rest). The subscription
     * lasts until closed and is re-established after reconnects. Messages arrive in order on one
     * thread per subscription.
     *
     * @param pattern the subject pattern
     * @param queue a queue group whose members share messages, or null
     * @param handler called for every message
     * @return completes once the server first accepted the subscription; fails when it refuses it
     */
    public CompletableFuture<Subscription> subscribe(String pattern, String queue, Consumer<Message> handler) {
        return subscribe(pattern, queue, SubscriptionScope.CLUSTER, handler);
    }

    /**
     * {@link #subscribe(String, String, Consumer)} with a scope. A server from before scopes refuses
     * {@link SubscriptionScope#LOCAL}.
     *
     * @param pattern the subject pattern
     * @param queue a queue group whose members share messages, or null
     * @param scope which peers of a cluster the subscription hears
     * @param handler called for every message
     * @return completes once the server first accepted the subscription; fails when it refuses it
     */
    public CompletableFuture<Subscription> subscribe(
            String pattern, String queue, SubscriptionScope scope, Consumer<Message> handler) {
        Managed managed = new Managed(pattern, queue, Objects.requireNonNull(scope, "scope"), handler);
        subscriptions.add(managed);
        Session current = session;
        if (current != null) {
            readers.execute(() -> {
                try {
                    managed.establish(current);
                } catch (IOException e) {
                    // The connection is failing; the reconnect establishes it again.
                }
            });
        }
        return managed.first;
    }

    /**
     * Append a record to a retained stream and wait for the stream's durability boundary. This does
     * not also publish to live subscribers.
     *
     * @param stream a configured stream
     * @param subject a literal subject the stream stores
     * @param payload at most the stream's {@code max_payload_bytes}
     * @return the stored record's sequence and time; fails with {@link MessagingException} when Rift
     *     refuses it (an unknown stream, a subject the stream does not store)
     */
    public CompletableFuture<StoredRecord> streamPublish(String stream, String subject, byte[] payload) {
        return streamPublish(stream, subject, null, payload);
    }

    /**
     * @param stream a configured stream
     * @param subject a literal subject the stream stores
     * @param reply an {@code _INBOX.} subject covered by this identity's subscribe grants, stored with
     *     the record, or null
     * @param payload at most the stream's {@code max_payload_bytes}
     * @return the stored record's sequence and time
     */
    public CompletableFuture<StoredRecord> streamPublish(String stream, String subject, String reply, byte[] payload) {
        streamName(stream, "stream");
        Session current = session;
        return current == null ? notConnected() : current.streamPublish(stream, subject, reply, payload);
    }

    /**
     * Create or reopen a durable consumer owned by this identity. Reopening with the same name and
     * settings resumes where the consumer left off.
     *
     * @param stream a configured stream
     * @param name the consumer's name: 1 to 96 ASCII letters, digits, {@code _}, {@code -} or
     *     {@code .}
     * @param config the consumer's settings
     * @return the consumer once Rift accepted it; fails with {@link MessagingException} when Rift
     *     refuses it (an unknown stream, different settings under the same name, too many consumers)
     */
    public CompletableFuture<StreamConsumer> consumer(String stream, String name, ConsumerConfig config) {
        streamName(stream, "stream");
        streamName(name, "name");
        Objects.requireNonNull(config, "config");
        Session current = session;
        if (current == null) {
            return notConnected();
        }
        return current.consumerOpen(stream, name, config).thenApply(opened -> new RemoteConsumer(stream, name));
    }

    private static void streamName(String name, String what) {
        Objects.requireNonNull(name, what);
        boolean valid = !name.isEmpty() && name.length() <= 96;
        for (int i = 0; valid && i < name.length(); i++) {
            char c = name.charAt(i);
            valid = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
                    || c == '_' || c == '-' || c == '.';
        }
        if (!valid) {
            throw new IllegalArgumentException(what + " must be 1 to 96 ASCII letters, digits, '_', '-' or '.'");
        }
    }

    /** A consumer Rift holds; every call runs on whichever connection is current. */
    private final class RemoteConsumer implements StreamConsumer {
        private final String stream;
        private final String name;

        RemoteConsumer(String stream, String name) {
            this.stream = stream;
            this.name = name;
        }

        @Override
        public String stream() {
            return stream;
        }

        @Override
        public String name() {
            return name;
        }

        @Override
        public CompletableFuture<List<Delivery>> fetch(int limit) {
            if (limit < 1 || limit > Frames.MAX_FETCH) {
                throw new IllegalArgumentException("fetch limit must be 1 to " + Frames.MAX_FETCH);
            }
            Session current = session;
            return current == null ? notConnected() : current.fetch(stream, name, limit);
        }

        @Override
        public CompletableFuture<Void> ack(long sequence) {
            Session current = session;
            return current == null ? notConnected() : current.ack(stream, name, sequence);
        }

        @Override
        public CompletableFuture<Void> delete() {
            Session current = session;
            return current == null ? notConnected() : current.consumerDelete(stream, name);
        }

        @Override
        public String toString() {
            return "StreamConsumer[stream=" + stream + ", name=" + name + "]";
        }
    }

    /**
     * @return a pipelined publisher on the current connection
     * @throws MessagingException when not connected
     * @throws IOException when the stream cannot be opened
     */
    public Publisher publisher() throws IOException {
        Session current = session;
        if (current == null) {
            throw new MessagingException("not connected", MessagingException.CLIENT);
        }
        return new Publisher(current.pipeline());
    }

    /** Close the connection and stop reconnecting. */
    @Override
    public void close() {
        closed = true;
        subscriptions.forEach(Managed::close);
        Session current = session;
        if (current != null) {
            current.close();
        }
        connector.interrupt();
        connected.completeExceptionally(new MessagingException("client closed", MessagingException.CLIENT));
        readers.shutdownNow();
        timers.shutdownNow();
    }

    private static <T> CompletableFuture<T> notConnected() {
        return CompletableFuture.failedFuture(new MessagingException("not connected", MessagingException.CLIENT));
    }

    private void run() {
        List<Address> addresses = settings.addresses();
        long delay = settings.minBackoff().toMillis();
        int index = 0;
        int failures = 0;
        while (!closed) {
            Session opened = null;
            try {
                opened = Session.open(settings, addresses.get(index), readers, timers);
                for (Managed managed : subscriptions) {
                    managed.establish(opened);
                }
            } catch (IOException | RuntimeException e) {
                if (opened != null) {
                    opened.close();
                }
                if (closed) {
                    return;
                }
                notify(listener -> listener.connectFailed(e));
                index = (index + 1) % addresses.size();
                failures++;
                // Try every address once before pausing.
                if (failures % addresses.size() == 0) {
                    if (!sleep(delay)) {
                        return;
                    }
                    delay = Math.min(delay * 2, settings.maxBackoff().toMillis());
                }
                continue;
            }
            delay = settings.minBackoff().toMillis();
            failures = 0;
            session = opened;
            connected.complete(null);
            notify(ConnectionListener::connected);
            Throwable cause;
            try {
                cause = opened.closed().get();
            } catch (InterruptedException e) {
                opened.close();
                return;
            } catch (ExecutionException e) {
                cause = e.getCause();
            }
            connected = new CompletableFuture<>();
            session = null;
            if (closed) {
                return;
            }
            Throwable reason = cause;
            notify(listener -> listener.disconnected(reason));
            if (addresses.size() > 1) {
                // Another peer is probably up: move on at once.
                index = (index + 1) % addresses.size();
            } else if (!sleep(delay)) {
                return;
            }
        }
    }

    /** Sleep about {@code millis}, with jitter; false when interrupted by {@link #close()}. */
    private boolean sleep(long millis) {
        try {
            Thread.sleep(millis / 2 + ThreadLocalRandom.current().nextLong(millis / 2 + 1));
            return true;
        } catch (InterruptedException e) {
            return false;
        }
    }

    private void notify(Consumer<ConnectionListener> event) {
        for (ConnectionListener listener : listeners) {
            try {
                event.accept(listener);
            } catch (RuntimeException e) {
                Thread current = Thread.currentThread();
                current.getUncaughtExceptionHandler().uncaughtException(current, e);
            }
        }
    }

    /**
     * Daemon threads whose context class loader is this library's, so that kwik's
     * {@code ServiceLoader} lookups find its providers when the library is shaded into a plugin.
     */
    private static ThreadFactory daemon(String name) {
        AtomicInteger count = new AtomicInteger();
        ClassLoader loader = MessagingClient.class.getClassLoader();
        return runnable -> {
            Thread thread = new Thread(runnable, name + "-" + count.incrementAndGet());
            thread.setDaemon(true);
            thread.setContextClassLoader(loader);
            return thread;
        };
    }

    /** A subscription that outlives connections. */
    private final class Managed implements Subscription {
        private final String pattern;
        private final String queue;
        private final SubscriptionScope scope;
        private final Consumer<Message> handler;
        private final CompletableFuture<Subscription> first = new CompletableFuture<>();
        private Session establishedOn;
        private Session.Active active;
        private boolean closedByUser;

        Managed(String pattern, String queue, SubscriptionScope scope, Consumer<Message> handler) {
            this.pattern = Objects.requireNonNull(pattern, "pattern");
            this.queue = queue;
            this.scope = scope;
            this.handler = Objects.requireNonNull(handler, "handler");
        }

        @Override
        public String pattern() {
            return pattern;
        }

        /**
         * Subscribe on {@code on} unless already done. A refusal by the server is final; other
         * failures belong to the connection and propagate.
         */
        synchronized void establish(Session on) throws IOException {
            if (closedByUser || establishedOn == on) {
                return;
            }
            try {
                active = on.subscribe(pattern, queue, scope, handler, cause -> ended(on));
                establishedOn = on;
                first.complete(this);
            } catch (MessagingException e) {
                if (e.code() == MessagingException.CLIENT) {
                    throw e;
                }
                subscriptions.remove(this);
                closedByUser = true;
                first.completeExceptionally(e);
            }
        }

        /** The server ended the stream on a live connection, for example a slow-consumer cut. */
        private synchronized void ended(Session on) {
            if (establishedOn == on) {
                establishedOn = null;
                active = null;
            }
            timers.schedule(() -> readers.execute(() -> {
                if (session == on) {
                    try {
                        establish(on);
                    } catch (IOException e) {
                        // The connection is failing; the reconnect establishes it again.
                    }
                }
            }), RESUBSCRIBE_DELAY.toMillis(), TimeUnit.MILLISECONDS);
        }

        @Override
        public synchronized void close() {
            closedByUser = true;
            subscriptions.remove(this);
            if (active != null) {
                active.close();
                active = null;
            }
        }
    }

    /** Configures a {@link MessagingClient}. */
    public static final class Builder {
        /** An address whose server name may still default. */
        private record Target(String host, int port, String serverName) {}

        private final List<Target> targets = new ArrayList<>();
        private String serverName;
        private final List<X509Certificate> certificates = new ArrayList<>();
        private Path certificateFile;
        private String token;
        private Duration connectTimeout = Duration.ofSeconds(10);
        private Duration minBackoff = Duration.ofSeconds(1);
        private Duration maxBackoff = Duration.ofSeconds(30);

        private Builder() {}

        /**
         * Connect to this address only, replacing any given before.
         *
         * @param host the address to send packets to: a host name or IP address
         * @param port the messaging listener's UDP port
         * @return this builder
         */
        public Builder address(String host, int port) {
            targets.clear();
            return addAddress(host, port);
        }

        /**
         * Add an address to try after the ones given before, such as another peer of a Rift cluster.
         * Its certificate is verified against {@link #serverName(String)}, else its host.
         *
         * @param host the address to send packets to: a host name or IP address
         * @param port the messaging listener's UDP port
         * @return this builder
         */
        public Builder addAddress(String host, int port) {
            targets.add(new Target(Objects.requireNonNull(host, "host"), port, null));
            return this;
        }

        /**
         * Add an address whose certificate names {@code serverName}.
         *
         * @param host the address to send packets to: a host name or IP address
         * @param port the messaging listener's UDP port
         * @param serverName the DNS name this address's certificate is verified against
         * @return this builder
         */
        public Builder addAddress(String host, int port, String serverName) {
            targets.add(new Target(Objects.requireNonNull(host, "host"), port,
                    Objects.requireNonNull(serverName, "serverName")));
            return this;
        }

        /**
         * @param serverName the DNS name certificates are verified against, for addresses without
         *     their own; defaults to each address's host
         * @return this builder
         */
        public Builder serverName(String serverName) {
            this.serverName = Objects.requireNonNull(serverName, "serverName");
            return this;
        }

        /**
         * @param pemFile a PEM file of CA or server certificates to trust
         * @return this builder
         */
        public Builder trust(Path pemFile) {
            this.certificateFile = Objects.requireNonNull(pemFile, "pemFile");
            return this;
        }

        /**
         * @param trusted CA or server certificates to trust
         * @return this builder
         */
        public Builder trust(Collection<X509Certificate> trusted) {
            certificates.addAll(trusted);
            return this;
        }

        /**
         * @param token this identity's secret
         * @return this builder
         */
        public Builder token(String token) {
            this.token = Objects.requireNonNull(token, "token");
            return this;
        }

        /**
         * Everything from the variables Rift sets for a managed server with a bus login, so one
         * configuration serves every instance of a group: {@code RIFT_BUS_ADDRESS} ({@code
         * host:port}), {@code RIFT_BUS_SERVER_NAME}, {@code RIFT_BUS_CERT} (the certificate to trust)
         * and {@code RIFT_BUS_TOKEN}. In a cluster, {@code RIFT_BUS_ADDRESSES} lists every peer
         * ({@code host:port} entries separated by commas) and takes the place of {@code
         * RIFT_BUS_ADDRESS}.
         *
         * @return this builder
         * @throws IllegalStateException when a variable is missing, naming it
         */
        public Builder fromEnvironment() {
            return fromEnvironment(System::getenv);
        }

        /**
         * {@link #fromEnvironment()} over any variable lookup, for tests.
         *
         * @param variables a variable's value, or null
         * @return this builder
         */
        public Builder fromEnvironment(java.util.function.Function<String, String> variables) {
            java.util.function.Function<String, String> required = name -> {
                String value = variables.apply(name);
                if (value == null || value.isEmpty()) {
                    throw new IllegalStateException(name + " is not set; Rift sets it for managed servers whose "
                            + "group or managed server has a messaging block");
                }
                return value;
            };
            String list = variables.apply("RIFT_BUS_ADDRESSES");
            String variable = list == null || list.isEmpty() ? "RIFT_BUS_ADDRESS" : "RIFT_BUS_ADDRESSES";
            String addresses = list == null || list.isEmpty() ? required.apply("RIFT_BUS_ADDRESS") : list;
            targets.clear();
            for (String address : addresses.split(",")) {
                address = address.strip();
                int colon = address.lastIndexOf(':');
                int port;
                try {
                    port = colon <= 0 ? -1 : Integer.parseInt(address.substring(colon + 1));
                } catch (NumberFormatException e) {
                    port = -1;
                }
                if (port < 0 || port > 65535) {
                    throw new IllegalStateException(variable + " must be host:port"
                            + (variable.equals("RIFT_BUS_ADDRESSES") ? " entries separated by commas" : "")
                            + ", got \"" + addresses + "\"");
                }
                addAddress(address.substring(0, colon).replace("[", "").replace("]", ""), port);
            }
            String serverName = variables.apply("RIFT_BUS_SERVER_NAME");
            if (serverName != null && !serverName.isEmpty()) {
                serverName(serverName);
            }
            trust(Path.of(required.apply("RIFT_BUS_CERT")));
            return token(required.apply("RIFT_BUS_TOKEN"));
        }

        /**
         * @param timeout the QUIC handshake and authentication deadline
         * @return this builder
         */
        public Builder connectTimeout(Duration timeout) {
            this.connectTimeout = Objects.requireNonNull(timeout, "timeout");
            return this;
        }

        /**
         * @param min the first reconnect delay
         * @param max the longest delay after repeated failures
         * @return this builder
         */
        public Builder reconnectDelay(Duration min, Duration max) {
            if (min.isNegative() || min.isZero() || max.compareTo(min) < 0) {
                throw new IllegalArgumentException("reconnect delays must satisfy 0 < min <= max");
            }
            this.minBackoff = min;
            this.maxBackoff = max;
            return this;
        }

        /**
         * Create the client; it starts connecting in the background.
         *
         * @return the client
         * @throws IOException when the certificate file cannot be read
         * @throws IllegalStateException when the address, token or trust is missing
         */
        public MessagingClient build() throws IOException {
            if (targets.isEmpty() || token == null) {
                throw new IllegalStateException("address and token are required");
            }
            if (token.getBytes(java.nio.charset.StandardCharsets.UTF_8).length > Frames.MAX_STRING) {
                throw new IllegalStateException("token exceeds " + Frames.MAX_STRING + " bytes");
            }
            List<X509Certificate> trusted = new ArrayList<>(certificates);
            if (certificateFile != null) {
                trusted.addAll(readCertificates(certificateFile));
            }
            if (trusted.isEmpty()) {
                throw new IllegalStateException("trust at least one certificate; the bus has no unverified mode");
            }
            List<Address> addresses = new ArrayList<>();
            for (Target target : targets) {
                String name = target.serverName() != null ? target.serverName()
                        : serverName != null ? serverName : target.host();
                addresses.add(new Address(target.host(), target.port(), name));
            }
            Settings settings = new Settings(List.copyOf(addresses), keyStore(trusted), token, connectTimeout,
                    minBackoff, maxBackoff);
            MessagingClient client = new MessagingClient(settings);
            client.connector.start();
            return client;
        }

        private static List<X509Certificate> readCertificates(Path file) throws IOException {
            try (InputStream in = Files.newInputStream(file)) {
                List<X509Certificate> result = new ArrayList<>();
                for (Certificate certificate : CertificateFactory.getInstance("X.509").generateCertificates(in)) {
                    result.add((X509Certificate) certificate);
                }
                return result;
            } catch (GeneralSecurityException e) {
                throw new IOException("cannot read certificates from " + file + ": " + e.getMessage(), e);
            }
        }

        private static KeyStore keyStore(List<X509Certificate> trusted) throws IOException {
            try {
                KeyStore store = KeyStore.getInstance(KeyStore.getDefaultType());
                store.load(null, null);
                for (int i = 0; i < trusted.size(); i++) {
                    store.setCertificateEntry("rift-" + i, trusted.get(i));
                }
                return store;
            } catch (GeneralSecurityException e) {
                throw new IOException("cannot build the trust store: " + e.getMessage(), e);
            }
        }
    }
}
