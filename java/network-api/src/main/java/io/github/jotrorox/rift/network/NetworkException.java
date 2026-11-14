package io.github.jotrorox.rift.network;

import java.util.Objects;

/** A request the proxy refused or could not answer. Futures of this API fail with this. */
public class NetworkException extends RuntimeException {
    /** Why a request failed. */
    public enum Reason {
        /** The proxy answered with an error: an unknown server, a player that is not this server's. */
        REFUSED,
        /** The server's bus login has no grant for the subject. */
        DENIED,
        /** No connection to the proxy, no answer in time, or nobody subscribed to a request. */
        UNAVAILABLE,
        /** An invalid argument, or an answer this client could not read. */
        INVALID
    }

    private final Reason reason;

    /**
     * @param reason why
     * @param message a human-readable explanation
     */
    public NetworkException(Reason reason, String message) {
        super(message);
        this.reason = Objects.requireNonNull(reason, "reason");
    }

    /**
     * @param reason why
     * @param message a human-readable explanation
     * @param cause the underlying failure
     */
    public NetworkException(Reason reason, String message, Throwable cause) {
        super(message, cause);
        this.reason = Objects.requireNonNull(reason, "reason");
    }

    /** @return why the request failed */
    public Reason reason() {
        return reason;
    }
}
