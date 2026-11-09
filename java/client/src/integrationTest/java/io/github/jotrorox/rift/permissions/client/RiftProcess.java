package io.github.jotrorox.rift.permissions.client;

import io.github.jotrorox.rift.messaging.MessagingClient;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
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
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * A real {@code rift} process with its permission store, admin service and messaging listener, a
 * fresh self-signed certificate, and these principals, mirroring {@code tests/permissions.rs}:
 *
 * <ul>
 *   <li>{@code server.survival-1}: the {@code backend} preset (plus {@code test.>} for the raw bus
 *       tests), speaking for backend {@code survival-1}, whose game port is {@link #backend()};
 *   <li>{@code server.lobby}: the {@code backend} preset with {@code control = true}, so it may
 *       write and run console commands as itself;
 *   <li>{@code raw}: plain {@code test.>} grants for the messaging tests.
 * </ul>
 *
 * The store is the SQLite file beside the configuration, so revisions survive {@link #stop()} and
 * {@link #launch()}; the messaging port stays the same across restarts.
 */
final class RiftProcess implements AutoCloseable {
    static final String SURVIVAL_TOKEN = "java-integration-survival-token-0123456789";
    static final String LOBBY_TOKEN = "java-integration-lobby-control-token-012345";
    static final String RAW_TOKEN = "java-integration-raw-token-0123456789abcdef";
    static final String ADMIN_TOKEN = "java-integration-admin-token-0123456789abc";

    private final Path directory;
    private final int port;
    private final X509Certificate certificate;
    private final ServerSocket backend;
    private final List<String> logs = new CopyOnWriteArrayList<>();
    private Process process;
    private InetSocketAddress front;
    private InetSocketAddress admin;

    private RiftProcess(Path directory, int port, X509Certificate certificate, ServerSocket backend) {
        this.directory = directory;
        this.port = port;
        this.certificate = certificate;
        this.backend = backend;
    }

    static Path binary() {
        return Path.of(System.getProperty("rift.binary", ""));
    }

    /** Create the configuration and start the process. */
    static RiftProcess start() throws Exception {
        RiftProcess rift = create();
        rift.launch();
        return rift;
    }

    /** Create the configuration without starting the process. */
    static RiftProcess create() throws Exception {
        Path directory = Files.createTempDirectory("rift-java-it");
        X509Certificate certificate = certificate(directory);
        int port;
        try (DatagramSocket socket = new DatagramSocket(0)) {
            port = socket.getLocalPort();
        }
        ServerSocket backend = new ServerSocket(0, 50, InetAddress.getLoopbackAddress());
        Files.writeString(directory.resolve("rift.lua"), """
                return {
                    listeners = { main = '127.0.0.1:0' },
                    backends = { ['survival-1'] = '127.0.0.1:%d', lobby = '127.0.0.1:1' },
                    routes = { main = 'survival-1' },
                    permissions = { contexts = { external = { 'region' } } },
                    admin = { listen = '127.0.0.1:0', permissions = { 'status', 'perms' } },
                    messaging = {
                        listen = '127.0.0.1:%d', certificate = 'cert.pem', private_key = 'key.pem',
                        principals = {
                            {
                                name = 'server.survival-1', token_env = 'RIFT_JAVA_SURVIVAL_TOKEN', preset = 'backend',
                                publish = { 'test.>' }, subscribe = { 'test.>' },
                            },
                            { name = 'server.lobby', token_env = 'RIFT_JAVA_LOBBY_TOKEN', preset = 'backend', control = true },
                            {
                                name = 'raw', token_env = 'RIFT_JAVA_RAW_TOKEN',
                                publish = { '_INBOX.>', 'test.>' }, subscribe = { 'test.>' },
                            },
                        },
                    },
                }
                """.formatted(backend.getLocalPort(), port));
        return new RiftProcess(directory, port, certificate, backend);
    }

    /** Start (or restart) the process on the same messaging port and wait for every listener. */
    void launch() throws IOException, InterruptedException {
        ProcessBuilder builder = new ProcessBuilder(binary().toAbsolutePath().toString(), "--config", "rift.lua")
                .directory(directory.toFile())
                .redirectOutput(ProcessBuilder.Redirect.DISCARD);
        Map<String, String> environment = builder.environment();
        environment.put("RIFT_JAVA_SURVIVAL_TOKEN", SURVIVAL_TOKEN);
        environment.put("RIFT_JAVA_LOBBY_TOKEN", LOBBY_TOKEN);
        environment.put("RIFT_JAVA_RAW_TOKEN", RAW_TOKEN);
        environment.put("RIFT_ADMIN_TOKEN", ADMIN_TOKEN);
        process = builder.start();
        BlockingQueue<String> lines = new LinkedBlockingQueue<>();
        Thread reader = new Thread(() -> drain(process.getErrorStream(), lines), "rift-stderr");
        reader.setDaemon(true);
        reader.start();
        front = null;
        admin = null;
        boolean bus = false;
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
            seen.add(line);
            if (line.startsWith("rift: listening on ")) {
                front = address(line.substring("rift: listening on ".length()));
            } else if (line.startsWith("rift: admin on ")) {
                admin = address(line.substring("rift: admin on ".length()));
            } else if (line.startsWith("rift: messaging QUIC on ")) {
                bus = true;
            }
            if (front != null && admin != null && bus) {
                return;
            }
        }
        throw new IllegalStateException("rift did not start every listener: " + seen);
    }

    private static InetSocketAddress address(String text) {
        int colon = text.lastIndexOf(':');
        return new InetSocketAddress(text.substring(0, colon), Integer.parseInt(text.substring(colon + 1).trim()));
    }

    private void drain(InputStream stream, BlockingQueue<String> lines) {
        try (BufferedReader reader = new BufferedReader(new InputStreamReader(stream, StandardCharsets.UTF_8))) {
            String line;
            while ((line = reader.readLine()) != null) {
                lines.add(line);
                logs.add(line);
            }
        } catch (IOException e) {
            // The process ended.
        }
    }

    /** Kill the process, keeping its configuration and store for {@link #launch()}. */
    void stop() {
        process.destroyForcibly();
        try {
            process.waitFor(10, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** @return the players' listener */
    InetSocketAddress front() {
        return front;
    }

    /** @return the game port of backend {@code survival-1}, where the proxy forwards players */
    ServerSocket backend() {
        return backend;
    }

    /** @return every line rift wrote to stderr, across restarts */
    List<String> logs() {
        return logs;
    }

    /** @return the bus certificate as a PEM file */
    Path certificateFile() {
        return directory.resolve("cert.pem");
    }

    /** @return the messaging port, unchanged across restarts */
    int port() {
        return port;
    }

    /**
     * Run {@code rift admin perms <args>} against this process.
     *
     * @return the exit code and the standard output, then the standard error
     */
    Result admin(String... args) throws IOException, InterruptedException {
        List<String> command = new ArrayList<>(List.of(binary().toAbsolutePath().toString(), "admin", "--address",
                admin.getHostString() + ":" + admin.getPort(), "perms"));
        command.addAll(List.of(args));
        ProcessBuilder builder = new ProcessBuilder(command);
        builder.environment().put("RIFT_ADMIN_TOKEN", ADMIN_TOKEN);
        Process cli = builder.start();
        String stdout = new String(cli.getInputStream().readAllBytes(), StandardCharsets.UTF_8);
        String stderr = new String(cli.getErrorStream().readAllBytes(), StandardCharsets.UTF_8);
        if (!cli.waitFor(30, TimeUnit.SECONDS)) {
            cli.destroyForcibly();
            throw new IllegalStateException("rift admin perms " + String.join(" ", args) + " hung");
        }
        return new Result(cli.exitValue(), stdout, stderr);
    }

    /** {@code rift admin perms <args>}, which must succeed; its output. */
    String perms(String... args) throws IOException, InterruptedException {
        Result result = admin(args);
        if (result.exit() != 0) {
            throw new AssertionError("perms " + String.join(" ", args) + " failed: " + result.stderr() + result.stdout());
        }
        return result.stdout();
    }

    /**
     * @param exit the exit code
     * @param stdout the standard output
     * @param stderr the standard error
     */
    record Result(int exit, String stdout, String stderr) {}

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
        if (process != null) {
            stop();
        }
        backend.close();
        try (var files = Files.walk(directory)) {
            files.sorted(java.util.Comparator.reverseOrder()).forEach(path -> path.toFile().delete());
        }
    }
}
