package io.github.jotrorox.rift.permissions;

/**
 * The proxy did not answer within the client's timeout. Every write future fails with this rather
 * than hanging, so {@code join()} is safe on a server thread. A timed-out write may still have been
 * applied: compare {@link RiftPermissions#revision()} before retrying.
 */
public class PermissionTimeoutException extends PermissionException {
    /** @param message what timed out */
    public PermissionTimeoutException(String message) {
        super(ErrorCode.UNAVAILABLE, message);
    }
}
