package io.github.jotrorox.rift.network;

/**
 * Whether a server that is up takes players. What the server says about itself, such as {@code
 * INGAME}, is {@link ServerInfo#serviceState()}.
 */
public enum ServerState {
    /** Up and accepting players. */
    RUNNING,
    /**
     * Up and keeping its players, but accepting no new ones: an operator drains it, or the server
     * asked for it itself.
     */
    DRAINING,
    /** A state this version of the API does not know; treat the server as not accepting players. */
    UNKNOWN
}
