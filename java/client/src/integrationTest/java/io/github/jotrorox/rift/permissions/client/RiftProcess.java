package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.messaging.MessagingClient;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.net.DatagramSocket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.Key;
import java.security.KeyStore;
import java.security.cert.X509Certificate;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import java.util.Map;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * A real {@code rift} process with the messaging listener enabled, a fresh self-signed certificate
 * and two principals: a backend with exactly the documented permission grants, and a stand-in for
 * the proxy's permission handlers (which do not exist yet) that answers {@code rift.perms.*}.
 */
final class RiftProcess implements AutoCloseable {
    static final String BACKEND_TOKEN = "java-integration-backend-token-0123456789";
    static final String STANDIN_TOKEN = "java-integration-standin-token-0123456789";

    private final Path directory;
    private final int port;
    private final X509Certificate certificate;
    private Process process;

    private RiftProcess(Path directory, int port, X509Certificate certificate) {
        this.directory = directory;
        this.port = port;
        this.certificate = certificate;
    }

    static Path binary() {
        return Path.of(System.getProperty("rift.binary", ""));
    }

    /** Create the configuration and start the process. */
    static RiftProcess start() throws Exception {
        Path directory = Files.createTempDirectory("rift-java-it");
        X509Certificate certificate = certificate(directory);
        int port;
        try (DatagramSocket socket = new DatagramSocket(0)) {
            port = socket.getLocalPort();
        }
        Files.writeString(directory.resolve("rift.lua"), """
                return {
                    listeners = { main = '127.0.0.1:0' },
                    backends = { lobby = '127.0.0.1:1' },
                    routes = { main = 'lobby' },
                    messaging = {
                        listen = '127.0.0.1:%d', certificate = 'cert.pem', private_key = 'key.pem',
                        principals = {
                            {
                                name = 'server.survival-1', token_env = 'RIFT_JAVA_BACKEND_TOKEN',
                                publish = {
                                    'rift.perms.hello', 'rift.perms.subject.*', 'rift.perms.groups.*',
                                    'rift.perms.apply', 'rift.perms.register', 'rift.perms.command',
                                    'rift.perms.context', 'test.>',
                                },
                                subscribe = { 'rift.perms.update', 'rift.perms.update.>', 'rift.events.permissions', 'test.>' },
                            },
                            {
                                name = 'proxy-standin', token_env = 'RIFT_JAVA_STANDIN_TOKEN',
                                publish = { 'rift.perms.>', '_INBOX.>', 'test.>' },
                                subscribe = { 'rift.perms.>', 'test.>' },
                            },
                        },
                    },
                }
                """.formatted(port));
        RiftProcess rift = new RiftProcess(directory, port, certificate);
        rift.launch();
        return rift;
    }

    /** Start (or restart) the process on the same port and wait for the messaging listener. */
    void launch() throws IOException, InterruptedException {
        ProcessBuilder builder = new ProcessBuilder(binary().toAbsolutePath().toString(), "--config", "rift.lua")
                .directory(directory.toFile())
                .redirectOutput(ProcessBuilder.Redirect.DISCARD);
        Map<String, String> environment = builder.environment();
        environment.put("RIFT_JAVA_BACKEND_TOKEN", BACKEND_TOKEN);
        environment.put("RIFT_JAVA_STANDIN_TOKEN", STANDIN_TOKEN);
        process = builder.start();
        BlockingQueue<String> lines = new LinkedBlockingQueue<>();
        Thread reader = new Thread(() -> drain(process.getErrorStream(), lines), "rift-stderr");
        reader.setDaemon(true);
        reader.start();
        List<String> seen = new ArrayList<>();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
        while (System.nanoTime() < deadline) {
            String line = lines.poll(200, TimeUnit.MILLISECONDS);
            if (line == null) {
                if (!process.isAlive()) {
                    throw new IllegalStateException("rift exited: " + seen);
                }
                continue;
            }
            if (line.startsWith("rift: messaging QUIC on ")) {
                return;
            }
            seen.add(line);
        }
        throw new IllegalStateException("rift did not start messaging: " + seen);
    }

    private static void drain(InputStream stream, BlockingQueue<String> lines) {
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(stream, StandardCharsets.UTF_8))) {
            String line;
            while ((line = reader.readLine()) != null) {
                lines.add(line);
            }
        } catch (IOException e) {
            // The process ended.
        }
    }

    /** Kill the process, keeping its configuration for {@link #launch()}. */
    void stop() {
        process.destroyForcibly();
        try {
            process.waitFor(10, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    MessagingClient connect(String token) throws IOException {
        return MessagingClient.builder()
                .address("127.0.0.1", port)
                .serverName("localhost")
                .trust(List.of(certificate))
                .token(token)
                .reconnectDelay(Duration.ofMillis(100), Duration.ofMillis(500))
                .build();
    }

    /** An EC P-256 certificate for {@code localhost}, made with the JDK's keytool. */
    private static X509Certificate certificate(Path directory) throws Exception {
        Path store = directory.resolve("keys.p12");
        Path keytool = Path.of(System.getProperty("java.home"), "bin", "keytool");
        Process keys = new ProcessBuilder(keytool.toString(), "-genkeypair", "-alias", "rift", "-keyalg", "EC",
                "-groupname", "secp256r1", "-sigalg", "SHA256withECDSA", "-dname", "CN=localhost",
                "-ext", "SAN=dns:localhost", "-validity", "2", "-storetype", "PKCS12", "-keystore", store.toString(),
                "-storepass", "changeit", "-keypass", "changeit")
                .redirectErrorStream(true)
                .start();
        String output = new String(keys.getInputStream().readAllBytes(), StandardCharsets.UTF_8);
        if (keys.waitFor() != 0) {
            throw new IllegalStateException("keytool failed: " + output);
        }
        KeyStore keyStore = KeyStore.getInstance("PKCS12");
        try (InputStream in = Files.newInputStream(store)) {
            keyStore.load(in, "changeit".toCharArray());
        }
        X509Certificate certificate = (X509Certificate) keyStore.getCertificate("rift");
        Key key = keyStore.getKey("rift", "changeit".toCharArray());
        Files.writeString(directory.resolve("cert.pem"), pem("CERTIFICATE", certificate.getEncoded()));
        Files.writeString(directory.resolve("key.pem"), pem("PRIVATE KEY", key.getEncoded()));
        return certificate;
    }

    private static String pem(String type, byte[] der) {
        return "-----BEGIN " + type + "-----\n"
                + Base64.getMimeEncoder(64, "\n".getBytes(StandardCharsets.US_ASCII)).encodeToString(der)
                + "\n-----END " + type + "-----\n";
    }

    @Override
    public void close() throws IOException {
        stop();
        try (var files = Files.walk(directory)) {
            files.sorted(java.util.Comparator.reverseOrder()).forEach(path -> path.toFile().delete());
        }
    }
}
