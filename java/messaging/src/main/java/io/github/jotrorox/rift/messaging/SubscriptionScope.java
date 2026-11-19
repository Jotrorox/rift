package io.github.jotrorox.rift.messaging;

/**
 * Which peers of a Rift cluster a subscription to a user subject hears. On a single Rift both behave
 * the same. Rift's own subjects keep their fixed scopes whatever a subscription asks for.
 */
public enum SubscriptionScope {
    /** Messages published on any peer (the default). */
    CLUSTER,
    /** Only messages published on the peer this client is connected to. */
    LOCAL
}
