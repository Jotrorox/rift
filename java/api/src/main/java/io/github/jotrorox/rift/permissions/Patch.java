package io.github.jotrorox.rift.permissions;

/**
 * What {@link Change.Bulk} does to each selected node. {@code delete} wins over every other field;
 * otherwise each non-null field is written. Combine single-field patches with {@link #and}.
 *
 * <pre>{@code
 * Patch.value(false).and(Patch.makePermanent())
 * }</pre>
 *
 * @param delete remove the node
 * @param value the new value of permission and inherit nodes, or null
 * @param contexts move the node to these contexts (replacing a node already there), or null
 * @param expiresAt the new expiry in Unix seconds, or null
 * @param permanent remove the expiry
 * @param source the new source, or null
 */
public record Patch(
        boolean delete, Boolean value, ContextSet contexts, Long expiresAt, boolean permanent, String source) {
    private static final Patch NONE = new Patch(false, null, null, null, false, null);

    public Patch {
        if (source != null) {
            Names.source(source);
        }
    }

    /** @return a patch removing every selected node */
    public static Patch remove() {
        return new Patch(true, null, null, null, false, null);
    }

    /**
     * @param value the new value
     * @return a patch setting permission and inherit values
     */
    public static Patch value(boolean value) {
        return new Patch(false, value, null, null, false, null);
    }

    /**
     * @param contexts the target contexts
     * @return a patch moving nodes to {@code contexts}
     */
    public static Patch contexts(ContextSet contexts) {
        return new Patch(false, null, contexts, null, false, null);
    }

    /**
     * @param epochSecond Unix seconds
     * @return a patch setting the expiry
     */
    public static Patch expiresAt(long epochSecond) {
        return new Patch(false, null, null, epochSecond, false, null);
    }

    /** @return a patch removing the expiry */
    public static Patch makePermanent() {
        return new Patch(false, null, null, null, true, null);
    }

    /**
     * @param source the new source
     * @return a patch setting the source
     */
    public static Patch source(String source) {
        return new Patch(false, null, null, null, false, source);
    }

    /**
     * @param other another patch
     * @return a patch with the fields of both; {@code other} wins where both set a field
     */
    public Patch and(Patch other) {
        return new Patch(
                delete || other.delete,
                other.value != null ? other.value : value,
                other.contexts != null ? other.contexts : contexts,
                other.expiresAt != null ? other.expiresAt : expiresAt,
                permanent || other.permanent,
                other.source != null ? other.source : source);
    }

    /** @return whether the patch sets nothing */
    public boolean isEmpty() {
        return equals(NONE);
    }
}
