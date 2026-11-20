package io.github.jotrorox.rift.messaging;

import java.io.IOException;
import java.io.OutputStream;
import java.net.ConnectException;
import java.net.SocketTimeoutException;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.function.Consumer;
import tech.kwik.core.QuicClientConnection;
import tech.kwik.core.QuicStream;

/** One authenticated QUIC connection. Each operation runs on its own bidirectional stream. */
final class Session {
    static final String ALPN = "rift-messaging/1";
    /** Deadline for a frame the server should answer promptly, like the server's I/O timeout. */
    static final Duration IO_DEADLINE = Duration.ofSeconds(30);
    /**
     * Idle timeout offered to the server, which offers 60 s and pings every 10 s; kwik pings at half
     * of it. A restarted server cannot be told apart by a stateless reset (its reset key is new), so
     * this bounds how long a vanished server goes unnoticed.
     */
    static final Duration IDLE_TIMEOUT = Duration.ofSeconds(15);
    private static final int HANDSHAKE_ATTEMPTS = 3;
    private static final String HANDSHAKE_ERROR = "Handshake error:";

    private final QuicClientConnection connection;
    private final ExecutorService readers;
    private final ScheduledExecutorService timers;
    private final CompletableFuture<Throwable> closed = new CompletableFuture<>();
    private final Object publishLock = new Object();
    private Pipeline shared;

    private Session(QuicClientConnection connection, ExecutorService readers, ScheduledExecutorService timers) {
        this.connection = connection;
        this.readers = readers;
        this.timers = timers;
    }

    static Session open(MessagingClient.Settings settings, MessagingClient.Address address, ExecutorService readers,
            ScheduledExecutorService timers) throws IOException {
        for (int attempt = 1; ; attempt++) {
            QuicClientConnection connection = QuicClientConnection.newBuilder()
                    .host(address.serverName())
                    .proxy(address.host())
                    .port(address.port())
                    .applicationProtocol(ALPN)
                    .customTrustStore(settings.trust())
                    .connectTimeout(settings.connectTimeout())
                    .maxIdleTimeout(IDLE_TIMEOUT)
                    .build();
            Session session = new Session(connection, readers, timers);
            connection.setConnectionListener(event -> session.closed.complete(
                    new MessagingException("connection closed: " + event.closeReason(), MessagingException.CLIENT)));
            try {
                connection.connect();
                connection.keepAlive(Integer.MAX_VALUE);
                session.authenticate(settings.token(), settings.connectTimeout());
                return session;
            } catch (ConnectException e) {
                connection.close();
                if (attempt >= HANDSHAKE_ATTEMPTS || !isHandshakeRace(e)) {
                    throw e;
                }
            } catch (IOException | RuntimeException e) {
                connection.close();
                throw e;
            }
        }
    }

    /**
     * Whether a failed handshake hit the race in kwik's TLS engine, which sends the ClientHello before
     * it starts waiting for the ServerHello. A fast server (typically on loopback) can answer in
     * between, and the engine then rejects the ServerHello, sometimes without recording a reason.
     * Nothing has been sent on the connection yet, so a fresh one is safe to try.
     */
    static boolean isHandshakeRace(ConnectException e) {
        String message = e.getMessage();
        if (message == null || !message.startsWith(HANDSHAKE_ERROR)) {
            return false;
        }
        String reason = message.substring(HANDSHAKE_ERROR.length()).strip();
        return reason.isEmpty() || reason.contains("unexpected server hello");
    }

    private void authenticate(String token, Duration deadline) throws IOException {
        QuicStream stream = connection.createStream(true);
        try (OutputStream out = stream.getOutputStream()) {
            out.write(new Frames.Writer(Frames.AUTH).string(token).frame());
        }
        Frames.expectOk(read(stream, Frames.MAX_STRING + 3, deadline));
    }

    /** Completes with the reason once the connection has ended. */
    CompletableFuture<Throwable> closed() {
        return closed;
    }

    void close() {
        connection.close();
        closed.complete(new MessagingException("client closed", MessagingException.CLIENT));
    }

    /** Read one frame, aborting the stream when the deadline passes first. */
    private byte[] read(QuicStream stream, int max, Duration deadline) throws IOException {
        ScheduledFuture<?> timer = timers.schedule(() -> stream.abortReading(0), deadline.toMillis(),
                TimeUnit.MILLISECONDS);
        try {
            return Frames.read(stream.getInputStream(), max);
        } catch (IOException e) {
            if (timer.isDone()) {
                throw new SocketTimeoutException("no frame within " + deadline.toMillis() + " ms");
            }
            throw e;
        } finally {
            timer.cancel(false);
        }
    }

    CompletableFuture<Message> request(String subject, byte[] payload, Duration timeout) {
        long millis = timeout.toMillis();
        if (millis < 1 || millis > 120_000) {
            throw new IllegalArgumentException("request timeout must be 1 to 120000 ms");
        }
        byte[] frame = new Frames.Writer(Frames.REQUEST).u32(millis).subject(subject).payload(payload).frame();
        CompletableFuture<Message> result = new CompletableFuture<>();
        readers.execute(() -> {
            try {
                QuicStream stream = connection.createStream(true);
                try (OutputStream out = stream.getOutputStream()) {
                    out.write(frame);
                }
                Frames.Reader reader = new Frames.Reader(read(stream, Frames.MAX_FRAME, timeout.plusSeconds(5)));
                int opcode = reader.u8();
                if (opcode == Frames.MESSAGE) {
                    result.complete(Frames.message(reader));
                } else if (opcode == Frames.ERROR) {
                    MessagingException error = Frames.error(reader);
                    result.completeExceptionally(Frames.REQUEST_TIMED_OUT.equals(error.getMessage())
                            ? new TimeoutException("no reply on " + subject + " within " + millis + " ms")
                            : error);
                } else {
                    throw MessagingException.protocol("expected MESSAGE, got opcode " + opcode);
                }
            } catch (SocketTimeoutException e) {
                result.completeExceptionally(new TimeoutException("no reply on " + subject + " within " + millis + " ms"));
            } catch (IOException | RuntimeException e) {
                result.completeExceptionally(e);
            }
        });
        return result;
    }

    /**
     * One frame on its own stream, answered by {@code frames}. The request side is finished with
     * {@code close}, which unlike kwik's {@code flush} does not fail when the server, done with the
     * request, has already stopped the stream.
     */
    private <T> CompletableFuture<T> command(byte[] frame, Reply<T> frames) {
        CompletableFuture<T> result = new CompletableFuture<>();
        try {
            readers.execute(() -> {
                try {
                    QuicStream stream = connection.createStream(true);
                    try (OutputStream out = stream.getOutputStream()) {
                        out.write(frame);
                    }
                    result.complete(frames.read(stream));
                } catch (SocketTimeoutException e) {
                    result.completeExceptionally(
                            new TimeoutException("no answer within " + IO_DEADLINE.toMillis() + " ms"));
                } catch (IOException | RuntimeException e) {
                    result.completeExceptionally(e);
                }
            });
        } catch (RuntimeException e) {
            // The client was closed and its executor with it.
            result.completeExceptionally(new MessagingException("client closed", MessagingException.CLIENT));
        }
        return result;
    }

    private interface Reply<T> {
        T read(QuicStream stream) throws IOException;
    }

    /** The body of the next frame, which must have {@code opcode}; the server's ERROR is thrown. */
    private Frames.Reader expect(QuicStream stream, int opcode) throws IOException {
        Frames.Reader reader = new Frames.Reader(read(stream, Frames.MAX_FRAME, IO_DEADLINE));
        int got = reader.u8();
        if (got == Frames.ERROR) {
            throw Frames.error(reader);
        }
        if (got != opcode) {
            throw MessagingException.protocol("expected opcode " + opcode + ", got " + got);
        }
        return reader;
    }

    private Void ok(QuicStream stream) throws IOException {
        expect(stream, Frames.OK).end();
        return null;
    }

    CompletableFuture<StoredRecord> streamPublish(String stream, String subject, String reply, byte[] payload) {
        byte[] frame = new Frames.Writer(Frames.STREAM_PUBLISH)
                .string(stream)
                .subject(subject)
                .string(reply == null ? "" : reply)
                .payload(payload)
                .frame();
        return command(frame, answer -> {
            Frames.Reader reader = expect(answer, Frames.STORED);
            StoredRecord record = new StoredRecord(reader.u64(), reader.u64());
            reader.end();
            return record;
        });
    }

    CompletableFuture<Void> consumerOpen(String stream, String name, ConsumerConfig config) {
        return command(
                new Frames.Writer(Frames.CONSUMER_OPEN)
                        .string(stream)
                        .string(name)
                        .subject(config.filterSubject())
                        .u64(config.startSequence())
                        .u32(config.ackWait().toMillis())
                        .u32(config.maxAckPending())
                        .frame(),
                this::ok);
    }

    CompletableFuture<List<Delivery>> fetch(String stream, String name, int limit) {
        byte[] frame = new Frames.Writer(Frames.CONSUMER_FETCH).string(stream).string(name).u32(limit).frame();
        return command(frame, answer -> {
            List<Delivery> deliveries = new ArrayList<>();
            while (true) {
                Frames.Reader reader = new Frames.Reader(read(answer, Frames.MAX_FRAME, IO_DEADLINE));
                int opcode = reader.u8();
                if (opcode == Frames.OK) {
                    reader.end();
                    return List.copyOf(deliveries);
                }
                if (opcode == Frames.ERROR) {
                    throw Frames.error(reader);
                }
                if (opcode != Frames.DELIVERY) {
                    throw MessagingException.protocol("expected DELIVERY, got opcode " + opcode);
                }
                if (deliveries.size() >= limit) {
                    throw MessagingException.protocol("server exceeded the fetch limit");
                }
                long sequence = reader.u64();
                long timestamp = reader.u64();
                long count = reader.u32();
                deliveries.add(new Delivery(sequence, timestamp, count, Frames.message(reader)));
            }
        });
    }

    CompletableFuture<Void> ack(String stream, String name, long sequence) {
        return command(new Frames.Writer(Frames.ACK).string(stream).string(name).u64(sequence).frame(), this::ok);
    }

    CompletableFuture<Void> consumerDelete(String stream, String name) {
        return command(new Frames.Writer(Frames.CONSUMER_DELETE).string(stream).string(name).frame(), this::ok);
    }

    /** PUBLISH_ACK on the shared ordered stream; completes on the server's OK. */
    CompletableFuture<Void> publish(String subject, String reply, byte[] payload) {
        byte[] frame = new Frames.Writer(Frames.PUBLISH_ACK)
                .subject(subject)
                .string(reply == null ? "" : reply)
                .payload(payload)
                .frame();
        Pipeline pipeline;
        synchronized (publishLock) {
            if (shared == null || !shared.healthy()) {
                try {
                    shared = pipeline();
                } catch (IOException e) {
                    return CompletableFuture.failedFuture(e);
                }
            }
            pipeline = shared;
        }
        CompletableFuture<Void> result = pipeline.send(frame, true);
        result.orTimeout(IO_DEADLINE.toMillis(), TimeUnit.MILLISECONDS).whenComplete((ignored, error) -> {
            // A lost acknowledgement leaves later ones misaligned; start a fresh stream.
            if (error instanceof TimeoutException) {
                pipeline.fail(error);
            }
        });
        return result;
    }

    Pipeline pipeline() throws IOException {
        return new Pipeline(connection.createStream(true), readers);
    }

    /**
     * Subscribe and wait for the server's OK. Messages then arrive in order on a reader thread;
     * {@code ended} is called once if the stream ends without {@link Active#close()}.
     */
    Active subscribe(String pattern, String queue, SubscriptionScope scope, Consumer<Message> handler,
            Consumer<Throwable> ended) throws IOException {
        QuicStream stream = connection.createStream(true);
        Frames.Writer subscribe = new Frames.Writer(Frames.SUBSCRIBE).subject(pattern).string(queue == null ? "" : queue);
        // The cluster default is left out, so servers from before scopes accept it.
        if (scope == SubscriptionScope.LOCAL) {
            subscribe.u8(Frames.SCOPE_LOCAL);
        }
        // No flush: kwik sends written data right away, and its flush only checks the stream state.
        // The server stops a refused subscription's stream right after its ERROR frame, so a flush
        // could fail on the stopped stream and hide the server's reason.
        stream.getOutputStream().write(subscribe.frame());
        try {
            Frames.expectOk(read(stream, Frames.MAX_FRAME, IO_DEADLINE));
        } catch (IOException e) {
            stream.resetStream(0);
            throw e;
        }
        Active active = new Active(stream);
        readers.execute(() -> {
            try {
                while (true) {
                    Frames.Reader reader = new Frames.Reader(Frames.read(stream.getInputStream(), Frames.MAX_FRAME));
                    int opcode = reader.u8();
                    if (opcode == Frames.ERROR) {
                        throw Frames.error(reader);
                    }
                    if (opcode != Frames.MESSAGE) {
                        throw MessagingException.protocol("expected MESSAGE, got opcode " + opcode);
                    }
                    Message message = Frames.message(reader);
                    try {
                        handler.accept(message);
                    } catch (RuntimeException e) {
                        Thread current = Thread.currentThread();
                        current.getUncaughtExceptionHandler().uncaughtException(current, e);
                    }
                }
            } catch (IOException e) {
                if (!active.closed) {
                    active.close();
                    ended.accept(e);
                }
            }
        });
        return active;
    }

    /** The stream behind one established subscription. */
    static final class Active {
        private final QuicStream stream;
        private volatile boolean closed;

        private Active(QuicStream stream) {
            this.stream = stream;
        }

        void close() {
            if (!closed) {
                closed = true;
                stream.abortReading(0);
                stream.resetStream(0);
            }
        }
    }
}
