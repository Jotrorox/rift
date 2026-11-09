package io.github.jotrorox.rift.permissions;

import java.util.Objects;
import java.util.UUID;

/** A user or a group, the owner of nodes. */
public sealed interface SubjectRef {
    /**
     * @param uuid the player
     * @return a user reference
     */
    static SubjectRef user(UUID uuid) {
        return new User(uuid);
    }

    /**
     * @param name the group; lowercased
     * @return a group reference
     */
    static SubjectRef group(String name) {
        return new Group(name);
    }

    /**
     * A player.
     *
     * @param uuid the player
     */
    record User(UUID uuid) implements SubjectRef {
        public User {
            Objects.requireNonNull(uuid, "uuid");
        }

        /** @return {@code user:<hex>} */
        @Override
        public String toString() {
            return "user:" + Uuids.toHex(uuid);
        }
    }

    /**
     * A group.
     *
     * @param name the normalised group name
     */
    record Group(String name) implements SubjectRef {
        public Group {
            name = Names.group(name);
        }

        /** @return {@code group:<name>} */
        @Override
        public String toString() {
            return "group:" + name;
        }
    }
}
