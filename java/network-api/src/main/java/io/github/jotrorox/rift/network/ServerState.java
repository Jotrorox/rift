package io.github.jotrorox.rift.network;

/** The state of a server that is up. */
public enum ServerState {
    /** Up and accepting players. */
    RUNNING,
    /** Up and keeping its players, but accepting no new ones. */
    DRAINING,
    /** A state this version of the API does not know; treat the server as not accepting players. */
    UNKNOWN
}
