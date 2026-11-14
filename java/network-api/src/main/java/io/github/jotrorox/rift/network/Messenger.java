package io.github.jotrorox.rift.network;

import java.time.Duration;
import java.util.concurrent.CompletableFuture;
import java.util.function.Consumer;

/**
 * Rift's message bus: subjects shared by the proxy's plugins, its configuration script and every
 * server with a bus login. Payloads are bytes, so both ends choose the format.
 *
 * <pre>{@code
 * Messenger bus = RiftNetwork.get().messages();
 * bus.subscribe("party.invite.*", message -> invites.handle(message.text()));
 * bus.publish("party.invite." + target, json.getBytes(StandardCharsets.UTF_8));
 * bus.request("friends.list", uuid.toString().getBytes(StandardCharsets.UTF_8), Duration.ofSeconds(2))
 *    .thenAccept(reply -> show(reply.text()));
 * }</pre>
 *
 * <p>Subjects are dot-separated tokens. A pattern may use {@code *} for one token and a trailing
 * {@code >} for the rest. The server's bus login needs a publish or subscribe grant for each
 * subject; without one the future fails with {@link NetworkException.Reason#DENIED}. Delivery is at
 * most once and only to subscribers that are connected.
 */
public interface Messenger {
    /**
     * @param subject a literal subject
     * @param payload at most 1 MiB
     * @return completes when the proxy accepted the message, whether or not anyone is subscribed
     */
    CompletableFuture<Void> publish(String subject, byte[] payload);

    /**
     * Send a request to one responder and wait for its reply.
     *
     * @param subject a literal subject
     * @param payload at most 1 MiB
     * @param timeout 1 ms to 120 s
     * @return the reply; fails with {@link NetworkTimeoutException} when none arrives in time and
     *     with {@link NetworkException.Reason#UNAVAILABLE} when nobody is subscribed
     */
    CompletableFuture<BusMessage> request(String subject, byte[] payload, Duration timeout);

    /**
     * Subscribe until the returned subscription is closed; it survives reconnects. Messages arrive
     * in order on one Rift thread per subscription, never on a server thread. Answer a request by
     * publishing to its {@link BusMessage#replyTo()}.
     *
     * @param pattern the subject pattern
     * @param handler called for every message
     * @return completes once the proxy accepted the subscription
     */
    CompletableFuture<Subscription> subscribe(String pattern, Consumer<BusMessage> handler);

    /**
     * Subscribe as a member of a queue group: each message goes to one member, so several servers
     * can share work.
     *
     * @param pattern the subject pattern
     * @param queue the queue group's name
     * @param handler called for the messages this member receives
     * @return completes once the proxy accepted the subscription
     */
    CompletableFuture<Subscription> subscribe(String pattern, String queue, Consumer<BusMessage> handler);
}
