package io.github.jotrorox.rift.messaging;

import java.io.IOException;
import java.io.OutputStream;
import java.net.SocketTimeoutException;
import java.time.Duration;
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

    static Session open(MessagingClient.Settings settings, ExecutorService readers, ScheduledExecutorService timers)
            throws IOException {
        QuicClientConnection connection = QuicClientConnection.newBuilder()
                .host(settings.serverName())
                .proxy(settings.host())
                .port(settings.port())
                .applicationProtocol(ALPN)
                .customTrustStore(settings.trust())
                .connectTimeout(settings.connectTimeout())
                .maxIdleTimeout(Duration.ofSeconds(60))
                .build();
        Session session = new Session(connection, readers, timers);
        connection.setConnectionListener(event -> session.closed.complete(
                new MessagingException("connection closed: " + event.closeReason(), MessagingException.CLIENT)));
        try {
            connection.connect();
            connection.keepAlive(Integer.MAX_VALUE);
            session.authenticate(settings.token(), settings.connectTimeout());
        } catch (IOException | RuntimeException e) {
            connection.close();
            throw e;
        }
        return session;
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
    Active subscribe(String pattern, String queue, Consumer<Message> handler, Consumer<Throwable> ended)
            throws IOException {
        QuicStream stream = connection.createStream(true);
        OutputStream out = stream.getOutputStream();
        out.write(new Frames.Writer(Frames.SUBSCRIBE).subject(pattern).string(queue == null ? "" : queue).frame());
        out.flush();
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
