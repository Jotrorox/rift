package io.github.jotrorox.rift.messaging;

import java.util.concurrent.CompletableFuture;

/**
 * A dedicated ordered stream for high-throughput publishing: {@link #publish} writes PUBLISH frames
 * without waiting, {@link #flush} confirms that every earlier one reached the broker. After any
 * failure the publisher is unusable; create a new one. It does not survive a reconnect.
 */
public final class Publisher implements AutoCloseable {
    private final Pipeline pipeline;

    Publisher(Pipeline pipeline) {
        this.pipeline = pipeline;
    }

    /**
     * Queue one publication.
     *
     * @param subject a literal subject
     * @param reply an {@code _INBOX.} reply subject, or null
     * @param payload at most 1 MiB
     * @throws MessagingException when an earlier operation failed
     */
    public void publish(String subject, String reply, byte[] payload) throws MessagingException {
        byte[] frame = new Frames.Writer(Frames.PUBLISH)
                .subject(subject)
                .string(reply == null ? "" : reply)
                .payload(payload)
                .frame();
        pipeline.send(frame, false);
        Throwable failure = pipeline.failure();
        if (failure != null) {
            throw failure instanceof MessagingException error
                    ? error
                    : new MessagingException("publisher failed: " + failure.getMessage(), MessagingException.CLIENT);
        }
    }

    /** @return completes when the broker accepted every earlier publication, or fails with its error */
    public CompletableFuture<Void> flush() {
        return pipeline.send(new Frames.Writer(Frames.PING).frame(), true);
    }

    @Override
    public void close() {
        pipeline.close();
    }
}
