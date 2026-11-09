package io.github.jotrorox.rift.messaging;

import java.io.IOException;
import java.io.OutputStream;
import java.util.ArrayDeque;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;
import tech.kwik.core.QuicStream;

/**
 * One ordered operation stream carrying PUBLISH, PUBLISH_ACK and PING frames. The server answers
 * PUBLISH_ACK and PING with OK in order and closes the stream with ERROR on the first failure,
 * which fails every unanswered frame.
 */
final class Pipeline {
    /** How long a failed write waits for the server's ERROR frame before failing on its own. */
    private static final long ERROR_GRACE_MS = 1_000;

    private final QuicStream stream;
    private final OutputStream out;
    private final ArrayDeque<CompletableFuture<Void>> pending = new ArrayDeque<>();
    private Throwable failure;

    Pipeline(QuicStream stream, Executor readers) {
        this.stream = stream;
        this.out = stream.getOutputStream();
        readers.execute(this::readLoop);
    }

    /**
     * Write one frame. With {@code acknowledged} the future completes on the server's OK, otherwise
     * once the frame is written.
     */
    synchronized CompletableFuture<Void> send(byte[] frame, boolean acknowledged) {
        CompletableFuture<Void> result = new CompletableFuture<>();
        if (failure != null) {
            result.completeExceptionally(failure);
            return result;
        }
        try {
            if (acknowledged) {
                pending.add(result);
            }
            out.write(frame);
            out.flush();
            if (!acknowledged) {
                result.complete(null);
            }
        } catch (IOException e) {
            writeFailed(e);
        }
        return result;
    }

    /**
     * The server stops the stream right after its ERROR frame, so a write can fail before the reader
     * has handed that frame over. Give the reader a moment, so callers see the server's reason (an
     * ACL denial, say) rather than the stopped stream; {@link #fail} keeps the first cause.
     */
    private void writeFailed(IOException e) {
        CompletableFuture.delayedExecutor(ERROR_GRACE_MS, TimeUnit.MILLISECONDS).execute(() -> fail(e));
    }

    synchronized boolean healthy() {
        return failure == null;
    }

    synchronized Throwable failure() {
        return failure;
    }

    void close() {
        fail(new MessagingException("publisher closed", MessagingException.CLIENT));
    }

    synchronized void fail(Throwable cause) {
        if (failure != null) {
            return;
        }
        failure = cause;
        for (CompletableFuture<Void> waiting : pending) {
            waiting.completeExceptionally(cause);
        }
        pending.clear();
        stream.abortReading(0);
        stream.resetStream(0);
    }

    private void readLoop() {
        try {
            while (true) {
                Frames.Reader reader = new Frames.Reader(Frames.read(stream.getInputStream(), Frames.MAX_FRAME));
                int opcode = reader.u8();
                if (opcode == Frames.ERROR) {
                    fail(Frames.error(reader));
                    return;
                }
                if (opcode != Frames.OK) {
                    throw MessagingException.protocol("expected OK, got opcode " + opcode);
                }
                reader.end();
                CompletableFuture<Void> answered;
                synchronized (this) {
                    answered = pending.poll();
                }
                if (answered == null) {
                    throw MessagingException.protocol("unexpected OK");
                }
                answered.complete(null);
            }
        } catch (IOException e) {
            fail(e);
        }
    }
}
