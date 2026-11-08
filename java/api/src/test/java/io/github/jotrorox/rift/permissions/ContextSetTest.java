package io.github.jotrorox.rift.permissions;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;
import java.util.Optional;
import org.junit.jupiter.api.Test;

class ContextSetTest {
    @Test
    void canonicalFormSortsAndLowercases() {
        ContextSet set = ContextSet.parse("World=Nether;server=survival;region=b;region=a");
        assertEquals("region=a;region=b;server=survival;world=nether", set.toString());
        assertEquals(set, ContextSet.parse(set.toString()));
        assertEquals(ContextSet.empty(), ContextSet.parse(""));
        assertEquals(Optional.of("survival"), set.get("server"));
        assertEquals(List.of("a", "b"), set.values("region"));
        assertEquals(Map.of("region", List.of("a", "b"), "server", List.of("survival"), "world", List.of("nether")),
                set.asMap());
    }

    @Test
    void sortsByCodePointLikeRust() {
        // UTF-16 order would put the supplementary character first.
        ContextSet set = ContextSet.builder().add("k", "😀").add("k", "Ａ").build();
        assertEquals(List.of("Ａ", "😀"), set.values("k"));
    }

    @Test
    void lowercasesOnlyAscii() {
        assertEquals("k=Éa", ContextSet.of("K", "ÉA").toString());
    }

    @Test
    void rejectsMalformedPairs() {
        for (String bad : List.of("server", "server=", "=x", "a b=c", "k=v=w", "k=v;", "k=\u0007")) {
            assertThrows(IllegalArgumentException.class, () -> ContextSet.parse(bad), bad);
        }
        assertThrows(IllegalArgumentException.class, () -> ContextSet.of("k".repeat(65), "v"));
        assertThrows(IllegalArgumentException.class, () -> ContextSet.of("k", "v".repeat(257)));
        assertThrows(IllegalArgumentException.class, () -> ContextSet.of("k", "é".repeat(129)), "bytes, not chars");
        assertEquals("dimension=minecraft:the_nether", ContextSet.of("dimension", "minecraft:the_nether").toString());
    }

    @Test
    void subsetUnionAndSatisfiable() {
        ContextSet node = ContextSet.of("server", "survival");
        ContextSet active = ContextSet.parse("server=survival;world=world");
        assertTrue(node.isSubsetOf(active));
        assertTrue(ContextSet.empty().isSubsetOf(active));
        assertFalse(active.isSubsetOf(node));
        ContextSet both = node.union(ContextSet.of("server", "lobby"));
        assertEquals("server=lobby;server=survival", both.toString());
        assertFalse(both.isSatisfiable());
        assertTrue(ContextSet.parse("region=a;region=b").isSatisfiable());
    }

    @Test
    void replaceAndRemove() {
        ContextSet set = ContextSet.parse("region=a;region=b;world=w");
        set = set.withReplaced("region", "C");
        assertEquals("region=c;world=w", set.toString());
        assertTrue(set.contains("region", "c"));
        assertEquals("region=c", set.without("WORLD").toString());
        assertTrue(set.without("region").without("world").isEmpty());
    }

    @Test
    void equalSetsAreEqualValues() {
        ContextSet a = ContextSet.builder().add("b", "2").add("a", "1").build();
        ContextSet b = ContextSet.parse("a=1;b=2");
        assertEquals(a, b);
        assertEquals(a.hashCode(), b.hashCode());
        assertEquals(a, a.with("A", "1"), "duplicates collapse");
    }
}
