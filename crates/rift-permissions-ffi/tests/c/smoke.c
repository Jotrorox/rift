/* Drives librift_perms the way a C host would; argv[1] is the JSON
 * configuration, pointing at an unreachable proxy with a fallback file. */
#include <stdio.h>
#include <string.h>

#include "rift_perms.h"

#define EXPECT(condition)                                                   \
    do {                                                                    \
        if (!(condition)) {                                                 \
            fprintf(stderr, "line %d: %s\n", __LINE__, #condition);         \
            return 1;                                                       \
        }                                                                   \
    } while (0)

static void on_snapshot(const uint8_t *uuid, uint64_t revision, void *user) {
    (void)uuid;
    (void)revision;
    *(int *)user += 1;
}

int main(int argc, char **argv) {
    char *err = NULL;
    EXPECT(argc == 2);
    EXPECT(rift_perms_connect("{not json", &err) == NULL);
    EXPECT(err != NULL);
    rift_perms_free(err);
    EXPECT(rift_perms_check(NULL, NULL, NULL, NULL) == -2);
    rift_perms_close(NULL);

    rift_perms *perms = rift_perms_connect(argv[1], &err);
    EXPECT(perms != NULL && err == NULL);
    int snapshots = 0;
    rift_perms_subscribe(perms, on_snapshot, &snapshots);

    uint8_t alice[16] = {0};
    alice[15] = 1;
    EXPECT(rift_perms_check(perms, alice, "spawn", NULL) == -2);
    EXPECT(rift_perms_join(perms, alice) == 0);
    EXPECT(snapshots == 1);
    EXPECT(rift_perms_check(perms, alice, "spawn", NULL) == 1);
    EXPECT(rift_perms_check(perms, alice, "essentials.fly", NULL) == 0);
    EXPECT(rift_perms_check(perms, alice, "other", "{\"world\": \"nether\"}") == -1);

    char *meta = rift_perms_meta(perms, alice, NULL);
    EXPECT(meta != NULL && strstr(meta, "\"prefix\":\"[Guest] \"") != NULL);
    rift_perms_free(meta);

    char *reply = NULL;
    EXPECT(rift_perms_apply(perms, NULL, "{\"op\": \"delete_track\", \"name\": \"ranks\"}", &reply) == -1);
    EXPECT(reply != NULL && strstr(reply, "\"code\":\"unavailable\"") != NULL);
    rift_perms_free(reply);

    rift_perms_leave(perms, alice);
    EXPECT(rift_perms_revision(perms) == 0);
    rift_perms_close(perms);
    puts("ok");
    return 0;
}
