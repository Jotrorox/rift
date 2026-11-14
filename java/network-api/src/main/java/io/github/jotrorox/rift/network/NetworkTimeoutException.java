package io.github.jotrorox.rift.network;

/**
 * The proxy, or the responder of a bus request, did not answer in time. A timed-out {@link
 * RiftNetwork#connect} may still have been queued.
 */
public class NetworkTimeoutException extends NetworkException {
    /** @param message what timed out */
    public NetworkTimeoutException(String message) {
        super(Reason.UNAVAILABLE, message);
    }
}
