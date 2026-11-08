package io.github.jotrorox.rift.permissions;

import java.util.Optional;

/**
 * A check result that keeps "not set" apart from "denied". {@link #UNDEFINED} lets the platform
 * apply its own default: Bukkit the registered permission's default, then operator status.
 */
public enum Tristate {
    TRUE,
    FALSE,
    UNDEFINED;

    /**
     * @param value a decided answer
     * @return {@link #TRUE} or {@link #FALSE}
     */
    public static Tristate of(boolean value) {
        return value ? TRUE : FALSE;
    }

    /**
     * @param value an answer, or null when undefined
     * @return the matching tristate
     */
    public static Tristate of(Boolean value) {
        return value == null ? UNDEFINED : of(value.booleanValue());
    }

    /**
     * @param fallback the answer when undefined
     * @return the answer, or {@code fallback}
     */
    public boolean asBoolean(boolean fallback) {
        return this == UNDEFINED ? fallback : this == TRUE;
    }

    /** @return the answer, or empty when undefined */
    public Optional<Boolean> asOptional() {
        return this == UNDEFINED ? Optional.empty() : Optional.of(this == TRUE);
    }

    /** @return whether an entry decided the check */
    public boolean isDefined() {
        return this != UNDEFINED;
    }
}
