package io.github.jotrorox.rift.permissions;

import java.util.function.Consumer;

/**
 * Permission events: {@link PermissionChangeEvent} for every committed write the server hears of,
 * and {@link SubjectUpdatedEvent} when a player's snapshot is replaced here. Listeners run in order
 * on one dedicated thread, never on a server thread; schedule back onto the server to touch game
 * state.
 */
public interface EventBus {
    /**
     * @param type {@link PermissionChangeEvent} or {@link SubjectUpdatedEvent}
     * @param listener called for each event of {@code type}
     * @param <E> the event type
     * @return a subscription that removes the listener
     * @throws IllegalArgumentException for any other type
     */
    <E> Subscription subscribe(Class<E> type, Consumer<? super E> listener);
}
