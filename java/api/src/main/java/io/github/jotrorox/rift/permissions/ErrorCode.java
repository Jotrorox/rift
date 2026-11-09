package io.github.jotrorox.rift.permissions;

/** Why the proxy refused a request. */
public enum ErrorCode {
    /** Malformed JSON, a missing field or a value that fails validation. */
    INVALID_REQUEST("invalid_request"),
    /** The backend or the named player may not do this. */
    FORBIDDEN("forbidden"),
    /** A named group, track or player does not exist. */
    NOT_FOUND("not_found"),
    /** A store limit would be exceeded. */
    LIMIT("limit"),
    /** The change cannot apply to the current state, such as promoting past a track's end. */
    CONFLICT("conflict"),
    /** The proxy cannot serve the request right now, or cannot be reached. */
    UNAVAILABLE("unavailable"),
    /** Anything else; also any code this version does not know. */
    INTERNAL("internal");

    private final String wireName;

    ErrorCode(String wireName) {
        this.wireName = wireName;
    }

    /** @return the JSON spelling */
    public String wireName() {
        return wireName;
    }

    /**
     * @param name a JSON spelling, or null
     * @return the code; {@link #INTERNAL} for null or unknown names
     */
    public static ErrorCode fromWireName(String name) {
        for (ErrorCode code : values()) {
            if (code.wireName.equals(name)) {
                return code;
            }
        }
        return INTERNAL;
    }
}
