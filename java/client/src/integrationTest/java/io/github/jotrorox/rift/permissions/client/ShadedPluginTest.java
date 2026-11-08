package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.github.jotrorox.rift.permissions.Uuids;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.UUID;
import org.junit.jupiter.api.Test;

/**
 * The relocated client inside the shaded RiftPerms jar, loaded by its own class loader the way
 * Paper loads a plugin, against the real proxy. The calling thread's context class loader sees
 * neither kwik nor the client, so this fails if kwik's {@code ServiceLoader} lookups or the merged
 * service files of the relocated QUIC stack are broken.
 */
class ShadedPluginTest {
    private static final String LIB = "io.github.jotrorox.rift.permissions.paper.lib.";

    @Test
    void shadedClientConnectsFromAnIsolatedClassLoader() throws Exception {
        assumeTrue(Files.isExecutable(RiftProcess.binary()), "build the proxy first: cargo build --bin rift");
        Path jar = Path.of(System.getProperty("riftperms.jar", ""));
        assumeTrue(Files.isRegularFile(jar), "build the plugin first: ./gradlew :rift-permissions-paper:shadowJar");
        UUID alice = MinecraftLogin.offlineUuid("Alice");
        ClassLoader previous = Thread.currentThread().getContextClassLoader();
        try (RiftProcess rift = RiftProcess.start();
                URLClassLoader plugin = new URLClassLoader("RiftPerms", new URL[] {jar.toUri().toURL()},
                        ClassLoader.getPlatformClassLoader())) {
            Thread.currentThread().setContextClassLoader(ClassLoader.getPlatformClassLoader());
            assertThrows(ClassNotFoundException.class,
                    () -> plugin.loadClass("io.github.jotrorox.rift.permissions.client.PermissionsClient"),
                    "the client is relocated");
            Class<?> messagingType = plugin.loadClass(LIB + "messaging.MessagingClient");
            Class<?> clientType = plugin.loadClass(LIB + "client.PermissionsClient");
            Class<?> contextType = plugin.loadClass("io.github.jotrorox.rift.permissions.ContextSet");
            assertSame(plugin, clientType.getClassLoader());
            assertSame(plugin, contextType.getClassLoader(), "the API comes from the plugin jar");

            Object busBuilder = messagingType.getMethod("builder").invoke(null);
            call(busBuilder, "address", new Class<?>[] {String.class, int.class}, "127.0.0.1", rift.port());
            call(busBuilder, "serverName", new Class<?>[] {String.class}, "localhost");
            call(busBuilder, "trust", new Class<?>[] {Path.class}, rift.certificateFile());
            call(busBuilder, "token", new Class<?>[] {String.class}, RiftProcess.SURVIVAL_TOKEN);
            Object bus = call(busBuilder, "build", new Class<?>[0]);
            Object builder = clientType.getMethod("builder").invoke(null);
            call(builder, "messaging", new Class<?>[] {messagingType}, bus);
            call(builder, "server", new Class<?>[] {String.class}, "survival-1");
            Object client = call(builder, "build", new Class<?>[0]);
            try {
                Method available = clientType.getMethod("isAvailable");
                await(() -> (Boolean) available.invoke(client));
                rift.perms("user", Uuids.toHex(alice), "permission", "set", "essentials.fly");
                Method revision = clientType.getMethod("revision");
                await(() -> (Long) revision.invoke(client) == 1);
                Object subject = clientType.getMethod("load", UUID.class).invoke(client, alice);
                ((java.util.concurrent.CompletableFuture<?>) subject).get(10, java.util.concurrent.TimeUnit.SECONDS);
                Object empty = contextType.getMethod("empty").invoke(null);
                Object answer = clientType.getMethod("check", UUID.class, String.class, contextType)
                        .invoke(client, alice, "essentials.fly", empty);
                assertEquals("TRUE", answer.toString());
            } finally {
                clientType.getMethod("close").invoke(client);
                messagingType.getMethod("close").invoke(bus);
            }
        } finally {
            Thread.currentThread().setContextClassLoader(previous);
        }
    }

    private static Object call(Object target, String name, Class<?>[] types, Object... args) throws Exception {
        return target.getClass().getMethod(name, types).invoke(target, args);
    }

    private interface Condition {
        boolean holds() throws Exception;
    }

    private static void await(Condition condition) throws Exception {
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (!condition.holds()) {
            if (System.nanoTime() > deadline) {
                throw new AssertionError("timed out");
            }
            Thread.sleep(20);
        }
    }
}
