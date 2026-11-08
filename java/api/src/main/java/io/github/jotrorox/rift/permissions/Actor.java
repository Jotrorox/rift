package io.github.jotrorox.rift.permissions;

import java.util.Objects;
import java.util.UUID;

/**
 * Who made a change, as the audit log records it. {@code toString()} is the wire form:
 * {@code uuid:<hex>}, {@code operator:<name>}, {@code lua:<plugin>}, {@code backend:<name>},
 * {@code cli}, {@code import} or {@code system}.
 */
public sealed interface Actor {
    /** The command line. */
    Actor CLI = new Cli();
    /** A LuckPerms import. */
    Actor IMPORT = new Import();
    /** Expiry sweeps, configuration mirroring and migrations. */
    Actor SYSTEM = new System();

    /**
     * @param text the wire form
     * @return the actor
     * @throws IllegalArgumentException for anything else; names are 1 to 96 bytes of
     *     {@code [A-Za-z0-9_.:-]}
     */
    static Actor parse(String text) {
        int colon = text.indexOf(':');
        if (colon < 0) {
            return switch (text) {
                case "cli" -> CLI;
                case "import" -> IMPORT;
                case "system" -> SYSTEM;
                default -> throw Names.invalid("actor", text,
                        "expected uuid:, operator:, lua:, backend:, cli, import or system");
            };
        }
        String name = text.substring(colon + 1);
        return switch (text.substring(0, colon)) {
            case "uuid" -> new Player(Uuids.parse(name));
            case "operator" -> new Operator(name);
            case "lua" -> new Lua(name);
            case "backend" -> new Backend(name);
            default -> throw Names.invalid("actor", text,
                    "expected uuid:, operator:, lua:, backend:, cli, import or system");
        };
    }

    private static String label(String name, String text) {
        if (!Names.validLabel(name)) {
            throw Names.invalid("actor", text, "names are 1 to 96 bytes of [A-Za-z0-9_.:-]");
        }
        return name;
    }

    /**
     * A player, for example one who typed {@code /perms}.
     *
     * @param uuid the player
     */
    record Player(UUID uuid) implements Actor {
        public Player {
            Objects.requireNonNull(uuid, "uuid");
        }

        @Override
        public String toString() {
            return "uuid:" + Uuids.toHex(uuid);
        }
    }

    /**
     * A dashboard or HTTP operator.
     *
     * @param name the operator
     */
    record Operator(String name) implements Actor {
        public Operator {
            label(name, "operator:" + name);
        }

        @Override
        public String toString() {
            return "operator:" + name;
        }
    }

    /**
     * A Lua extension.
     *
     * @param name the plugin
     */
    record Lua(String name) implements Actor {
        public Lua {
            label(name, "lua:" + name);
        }

        @Override
        public String toString() {
            return "lua:" + name;
        }
    }

    /**
     * A backend's bus principal.
     *
     * @param name the principal
     */
    record Backend(String name) implements Actor {
        public Backend {
            label(name, "backend:" + name);
        }

        @Override
        public String toString() {
            return "backend:" + name;
        }
    }

    /** See {@link #CLI}. */
    record Cli() implements Actor {
        @Override
        public String toString() {
            return "cli";
        }
    }

    /** See {@link #IMPORT}. */
    record Import() implements Actor {
        @Override
        public String toString() {
            return "import";
        }
    }

    /** See {@link #SYSTEM}. */
    record System() implements Actor {
        @Override
        public String toString() {
            return "system";
        }
    }
}
