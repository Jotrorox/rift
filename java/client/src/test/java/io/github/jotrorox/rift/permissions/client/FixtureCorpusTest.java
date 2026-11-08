package io.github.jotrorox.rift.permissions.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import io.github.jotrorox.rift.permissions.CompiledSubject;
import io.github.jotrorox.rift.permissions.ContextSet;
import io.github.jotrorox.rift.permissions.MetaView;
import io.github.jotrorox.rift.permissions.Tristate;
import io.github.jotrorox.rift.permissions.Uuids;
import io.github.jotrorox.rift.permissions.client.Json.Arr;
import io.github.jotrorox.rift.permissions.client.Json.Num;
import io.github.jotrorox.rift.permissions.client.Json.Obj;
import io.github.jotrorox.rift.permissions.client.Json.Str;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.UUID;
import java.util.function.Function;
import java.util.stream.Stream;
import org.junit.jupiter.api.DynamicTest;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.TestFactory;

/**
 * Runs the shared corpus in {@code tests/fixtures/permissions}: every {@code resolve} fixture's
 * checks and meta expectations against its compiled subjects, and every {@code wire} round trip.
 * {@code luckperms} fixtures belong to the importer, which the Java side does not have.
 */
class FixtureCorpusTest {
    /** Round trips by wire fixture {@code type}: parse as that type, serialise again. */
    private static final Map<String, Function<Json, Json>> WIRE = Map.ofEntries(
            Map.entry("compiled_subject", j -> Codec.toJson(Codec.readCompiledSubject(j))),
            Map.entry("node", j -> Codec.toJson(Codec.readNode(j))),
            Map.entry("nodes", j -> new Arr(Codec.readNodes(j).stream().map(Codec::toJson).toList())),
            Map.entry("context_set", j -> Codec.toJson(Codec.readContextSet(j))),
            Map.entry("change", j -> Codec.toJson(Codec.readChange(j))),
            Map.entry("changes", j -> new Arr(Codec.readChanges(j).stream().map(Codec::toJson).toList())),
            Map.entry("change_event", j -> Codec.toJson(Codec.readChangeEvent(j))),
            Map.entry("applied", j -> Codec.toJson(Codec.readApplied(j))),
            Map.entry("hello_request", j -> Codec.toJson(Codec.readHelloRequest(j))),
            Map.entry("hello_reply", j -> Codec.toJson(Codec.readHelloReply(j))),
            Map.entry("subject_get_request", j -> Codec.toJson(Codec.readSubjectGetRequest(j))),
            Map.entry("subject_batch_request", j -> Codec.toJson(Codec.readSubjectBatchRequest(j))),
            Map.entry("subject_batch_reply", j -> Codec.toJson(Codec.readSubjectBatchReply(j))),
            Map.entry("groups_get_request", j -> Codec.toJson(Codec.readGroupsGetRequest(j))),
            Map.entry("groups_reply", j -> Codec.toJson(Codec.readGroupsReply(j))),
            Map.entry("apply_request", j -> Codec.toJson(Codec.readApplyRequest(j))),
            Map.entry("register_request", j -> Codec.toRegisterJson(Codec.readRegisterRequest(j))),
            Map.entry("command_request", j -> Codec.toJson(Codec.readCommandRequest(j))),
            Map.entry("command_reply", j -> Codec.toJson(Codec.readCommandReply(j))),
            Map.entry("context_report", j -> Codec.toJson(Codec.readContextReport(j))),
            Map.entry("reply", j -> Codec.toJson(Codec.readReply(j, Function.identity()), Function.identity())),
            Map.entry("tristates", j -> new Arr(((Arr) j).items().stream()
                    .map(item -> Codec.toJson(Codec.readTristate(item))).toList())),
            Map.entry("meta_view", j -> Codec.toJson(Codec.readMetaView(j))),
            Map.entry("trace", j -> Codec.toJson(Codec.readTrace(j))));

    static List<Path> fixtures() throws IOException {
        Path directory = Path.of(System.getProperty("rift.fixtures"));
        try (Stream<Path> files = Files.list(directory)) {
            return files.filter(path -> path.toString().endsWith(".json")).sorted().toList();
        }
    }

    static Obj read(Path path) throws IOException {
        return (Obj) Json.parse(Files.readString(path, StandardCharsets.UTF_8));
    }

    static String string(Json json) {
        return ((Str) json).value();
    }

    @TestFactory
    Stream<DynamicTest> corpus() throws IOException {
        List<DynamicTest> tests = new ArrayList<>();
        for (Path path : fixtures()) {
            Obj fixture = read(path);
            String kind = string(fixture.get("kind"));
            String stem = path.getFileName().toString().replace(".json", "");
            assertTrue(stem.endsWith(string(fixture.get("name"))), stem + ": name must match the file name");
            switch (kind) {
                case "resolve" -> tests.add(DynamicTest.dynamicTest(stem, () -> resolve(fixture)));
                case "wire" -> tests.add(DynamicTest.dynamicTest(stem, () -> wire(fixture)));
                case "luckperms" -> {
                    // The LuckPerms importer runs on the proxy only.
                }
                default -> fail(stem + ": unknown fixture kind " + kind);
            }
        }
        return tests.stream();
    }

    @Test
    void corpusCoversEveryKind() throws IOException {
        Map<String, Integer> kinds = new HashMap<>();
        for (Path path : fixtures()) {
            kinds.merge(string(read(path).get("kind")), 1, Integer::sum);
        }
        assertTrue(kinds.getOrDefault("resolve", 0) >= 25, kinds.toString());
        assertTrue(kinds.getOrDefault("wire", 0) >= 40, kinds.toString());
    }

    private static long number(Json json) {
        return ((Num) json).value().longValueExact();
    }

    private static void resolve(Obj fixture) {
        long fixtureNow = number(fixture.get("now"));
        Map<UUID, CompiledSubject> compiled = new HashMap<>();
        ((Obj) fixture.get("compiled")).members().forEach((uuid, subject) -> {
            CompiledSubject parsed = Codec.readCompiledSubject(subject);
            assertEquals(Uuids.parse(uuid), parsed.uuid());
            compiled.put(parsed.uuid(), parsed);
        });
        Json checks = fixture.get("checks");
        if (checks != null) {
            for (Json item : ((Arr) checks).items()) {
                Obj check = (Obj) item;
                CompiledSubject subject = compiled.get(Uuids.parse(string(check.get("user"))));
                String node = string(check.get("node"));
                ContextSet contexts = check.get("contexts") == null
                        ? ContextSet.empty() : Codec.readContextSet(check.get("contexts"));
                long now = check.get("now") == null ? fixtureNow : number(check.get("now"));
                Tristate expected = Codec.readTristate(check.get("expect"));
                Tristate got = Resolver.check(subject, node, contexts, now);
                assertEquals(expected, got, "check " + node + " in {" + contexts + "} at " + now);
                assertEquals(got, Resolver.trace(subject, node, contexts, now).result(), "trace of " + node);
            }
        }
        Json meta = fixture.get("meta");
        if (meta != null) {
            for (Json item : ((Arr) meta).items()) {
                Obj expectation = (Obj) item;
                CompiledSubject subject = compiled.get(Uuids.parse(string(expectation.get("user"))));
                ContextSet contexts = expectation.get("contexts") == null
                        ? ContextSet.empty() : Codec.readContextSet(expectation.get("contexts"));
                long now = expectation.get("now") == null ? fixtureNow : number(expectation.get("now"));
                MetaView expected = Codec.readMetaView(expectation.get("expect"));
                assertEquals(expected, Resolver.meta(subject, contexts, now), "meta in {" + contexts + "}");
            }
        }
    }

    private static void wire(Obj fixture) {
        String type = string(fixture.get("type"));
        Function<Json, Json> roundTrip = WIRE.get(type);
        if (roundTrip == null) {
            fail("unknown wire type " + type);
        }
        Json json = fixture.get("json");
        if (fixture.get("error") instanceof Json.Bool error && error.value()) {
            assertThrows(IllegalArgumentException.class, () -> roundTrip.apply(json), "expected a parse error");
            return;
        }
        Json expect = fixture.get("expect") == null ? json : fixture.get("expect");
        assertEquals(expect, roundTrip.apply(json));
    }
}
