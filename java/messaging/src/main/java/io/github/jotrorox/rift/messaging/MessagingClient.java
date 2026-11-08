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
 * publish, request/reply and subscriptions.
 *
 * <p>The client connects in the background and reconnects with exponential backoff after a
 * failure; subscriptions are re-established on every new connection before listeners hear
 * {@link ConnectionListener#connected()}. Publications and requests fail fast while disconnected
 * and are never retried automatically: a failed publication may still have reached the broker.
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

    record Settings(
            String host,
            int port,
            String serverName,
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
        Managed managed = new Managed(pattern, queue, handler);
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
        long delay = settings.minBackoff().toMillis();
        while (!closed) {
            Session opened = null;
            try {
                opened = Session.open(settings, readers, timers);
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
                if (!sleep(delay)) {
                    return;
                }
                delay = Math.min(delay * 2, settings.maxBackoff().toMillis());
                continue;
            }
            delay = settings.minBackoff().toMillis();
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
            if (!sleep(delay)) {
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
        private final Consumer<Message> handler;
        private final CompletableFuture<Subscription> first = new CompletableFuture<>();
        private Session establishedOn;
        private Session.Active active;
        private boolean closedByUser;

        Managed(String pattern, String queue, Consumer<Message> handler) {
            this.pattern = Objects.requireNonNull(pattern, "pattern");
            this.queue = queue;
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
                active = on.subscribe(pattern, queue, handler, cause -> ended(on));
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
        private String host;
        private int port;
        private String serverName;
        private final List<X509Certificate> certificates = new ArrayList<>();
        private Path certificateFile;
        private String token;
        private Duration connectTimeout = Duration.ofSeconds(10);
        private Duration minBackoff = Duration.ofSeconds(1);
        private Duration maxBackoff = Duration.ofSeconds(30);

        private Builder() {}

        /**
         * @param host the address to send packets to: a host name or IP address
         * @param port the messaging listener's UDP port
         * @return this builder
         */
        public Builder address(String host, int port) {
            this.host = Objects.requireNonNull(host, "host");
            this.port = port;
            return this;
        }

        /**
         * @param serverName the DNS name the certificate is verified against; defaults to the host
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
            if (host == null || token == null) {
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
            Settings settings = new Settings(host, port, serverName == null ? host : serverName, keyStore(trusted),
                    token, connectTimeout, minBackoff, maxBackoff);
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
