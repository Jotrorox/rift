package io.github.jotrorox.rift.permissions;

/** Node kinds, with the storage codes and JSON names of the protocol. */
public enum NodeKind {
    PERMISSION("permission"),
    INHERIT("inherit"),
    PREFIX("prefix"),
    SUFFIX("suffix"),
    META("meta"),
    WEIGHT("weight"),
    DISPLAY_NAME("display_name");

    private final String wireName;

    NodeKind(String wireName) {
        this.wireName = wireName;
    }

    /** @return the stable storage code: {@code permission} 0 to {@code display_name} 6 */
    public int code() {
        return ordinal();
    }

    /** @return the JSON and command spelling, such as {@code display_name} */
    public String wireName() {
        return wireName;
    }

    /** @return whether the kind is valid on groups only ({@code weight}, {@code display_name}) */
    public boolean groupOnly() {
        return this == WEIGHT || this == DISPLAY_NAME;
    }

    /**
     * @param name a JSON spelling
     * @return the kind
     * @throws IllegalArgumentException for an unknown name
     */
    public static NodeKind fromWireName(String name) {
        for (NodeKind kind : values()) {
            if (kind.wireName.equals(name)) {
                return kind;
            }
        }
        throw Names.invalid("node kind", name, "unknown kind");
    }
}
