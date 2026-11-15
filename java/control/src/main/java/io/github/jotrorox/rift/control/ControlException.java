package io.github.jotrorox.rift.control;

import io.github.jotrorox.rift.messaging.MessagingException;

/** Rift received a control command and refused it, for example an unknown backend. */
public class ControlException extends MessagingException {
    private final String command;

    /**
     * @param command the control command, such as {@code drain}
     * @param message Rift's reason
     */
    public ControlException(String command, String message) {
        super(command + ": " + message, OPERATION);
        this.command = command;
    }

    /** @return the control command, such as {@code drain} */
    public String command() {
        return command;
    }
}
