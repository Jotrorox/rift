package io.github.jotrorox.rift.messaging;

import java.io.IOException;

/** An ERROR frame from the server, a protocol violation, or a missing connection. */
public class MessagingException extends IOException {
    /** Raised by this client: no connection, or a malformed frame from the server. */
    public static final int CLIENT = 0;
    /** The server's code for an ACL or authentication denial. */
    public static final int PERMISSION = 1;
    /** The server's code for an operation or protocol failure. */
    public static final int OPERATION = 2;

    private final int code;

    /**
     * @param message the explanation
     * @param code {@link #CLIENT}, {@link #PERMISSION} or {@link #OPERATION}
     */
    public MessagingException(String message, int code) {
        super(message);
        this.code = code;
    }

    static MessagingException protocol(String message) {
        return new MessagingException(message, CLIENT);
    }

    /** @return {@link #CLIENT}, {@link #PERMISSION} or {@link #OPERATION} */
    public int code() {
        return code;
    }
}
