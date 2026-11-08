package io.github.jotrorox.rift.permissions.test;

import static org.junit.jupiter.api.Assertions.assertEquals;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.Node;
import io.github.jotrorox.rift.permissions.SubjectRef;
import io.github.jotrorox.rift.permissions.Uuids;
import io.github.jotrorox.rift.permissions.client.Codec;
import io.github.jotrorox.rift.permissions.client.Json;
import io.github.jotrorox.rift.permissions.client.Json.Num;
import io.github.jotrorox.rift.permissions.client.Json.Obj;
import io.github.jotrorox.rift.permissions.client.Json.Str;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.stream.Stream;
import org.junit.jupiter.api.DynamicTest;
import org.junit.jupiter.api.TestFactory;

/**
 * Stage 1 against the corpus: compiling every {@code resolve} fixture's users must reproduce its
 * {@code compiled} member exactly, entry order included, as the Rust compiler does.
 */
class CompilerFixtureTest {
    @TestFactory
    Stream<DynamicTest> compilesLikeTheProxy() throws IOException {
        Path directory = Path.of(System.getProperty("rift.fixtures"));
        try (Stream<Path> files = Files.list(directory)) {
            return files.filter(path -> path.getFileName().toString().startsWith("resolve-"))
                    .sorted()
                    .map(path -> DynamicTest.dynamicTest(path.getFileName().toString(), () -> compile(path)))
                    .toList()
                    .stream();
        }
    }

    private static void compile(Path path) throws IOException {
        Obj fixture = (Obj) Json.parse(Files.readString(path, StandardCharsets.UTF_8));
        long now = ((Num) fixture.get("now")).value().longValueExact();
        Map<String, Compiler.Group> groups = new HashMap<>();
        if (fixture.get("groups") instanceof Obj table) {
            table.members().forEach((name, value) -> {
                Obj group = (Obj) value;
                String normalized = new SubjectRef.Group(name).name();
                int weight = group.get("weight") == null ? 0 : ((Num) group.get("weight")).value().intValueExact();
                String displayName = group.get("display_name") instanceof Str text ? text.value() : null;
                groups.put(normalized, new Compiler.Group(normalized, weight, displayName, nodes(group)));
            });
        }
        Obj compiled = (Obj) fixture.get("compiled");
        ((Obj) fixture.get("users")).members().forEach((hex, value) -> {
            Obj user = (Obj) value;
            UUID uuid = Uuids.parse(hex);
            String primary = user.get("primary_group") instanceof Str text ? text.value() : null;
            CompiledSubject expected = Codec.readCompiledSubject(compiled.get(hex));
            assertEquals(expected, Compiler.compile(new Compiler.User(uuid, primary, nodes(user)), groups, now, 1),
                    "compiled subject of " + hex);
        });
    }

    private static List<Node> nodes(Obj subject) {
        return subject.get("nodes") == null ? List.of() : Codec.readNodes(subject.get("nodes"));
    }
}
