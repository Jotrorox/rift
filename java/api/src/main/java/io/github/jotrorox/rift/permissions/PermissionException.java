package io.github.jotrorox.rift.permissions;

import java.util.Objects;

/** A request the proxy refused or could not answer. Write futures fail with this. */
public class PermissionException extends RuntimeException {
    private final ErrorCode code;

    /**
     * @param code why
     * @param message a human-readable explanation
     */
    public PermissionException(ErrorCode code, String message) {
        super(message);
        this.code = Objects.requireNonNull(code, "code");
    }

    /**
     * @param code why
     * @param message a human-readable explanation
     * @param cause the underlying failure
     */
    public PermissionException(ErrorCode code, String message, Throwable cause) {
        super(message, cause);
        this.code = Objects.requireNonNull(code, "code");
    }

    /** @return why the request failed */
    public ErrorCode code() {
        return code;
    }
}
