// Small helpers for writing Rift plugins in C on top of the generated
// `rift_plugin.h` bindings. Everything here is `static inline`; include this
// header instead of (or after) `rift_plugin.h`.
//
// Ownership rules of the generated bindings (wit-bindgen C):
//
// - Handler arguments are owned by your handler. Free them before returning,
//   for example with `rift_context_free(ctx)`. The weak defaults in
//   `rift_plugin_defaults.c` do this for hooks you do not implement.
// - Values you return from a handler are freed by the bindings after the
//   host has copied them (`free()` on every non-empty string and list). They
//   must therefore be heap-allocated: use `rift_string_dup`, `rift_string_fmt`
//   and the `rift_action_*` and `rift_registration_*` helpers, never
//   `rift_str("literal")`.
// - Arguments you pass to host imports are borrowed: `rift_str("literal")` is
//   fine there and nothing is freed.
// - Results the host hands back from imports (`ret` and `err` out-params) are
//   owned by you: free them with `rift_string_free`, `rift_bytes_free` or the
//   generated `rift_plugin_*_free` functions.
#ifndef RIFT_H
#define RIFT_H

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "rift_plugin.h"

#ifdef __cplusplus
extern "C" {
#endif

// Short names for the types plugins touch most.
typedef rift_plugin_string_t rift_string_t;
typedef rift_plugin_list_u8_t rift_bytes_t;
typedef rift_plugin_list_string_t rift_string_list_t;
typedef rift_plugin_option_string_t rift_option_string_t;
typedef rift_plugin_types_context_t rift_context_t;
typedef rift_plugin_types_action_t rift_action_t;
typedef rift_plugin_types_registration_t rift_registration_t;
typedef rift_plugin_types_bus_message_t rift_bus_message_t;
typedef rift_plugin_types_hook_t rift_hook_t;
typedef rift_plugin_types_level_t rift_level_t;

// ---------------------------------------------------------------------------
// Allocation

// Aborts on allocation failure: a guest has no better recovery, and the host
// turns the trap into the manifest's `on_failure` policy.
static inline void *rift_alloc(size_t size) {
  void *ptr = malloc(size ? size : 1);
  if (!ptr) abort();
  return ptr;
}

static inline void *rift_realloc(void *ptr, size_t size) {
  void *grown = realloc(ptr, size ? size : 1);
  if (!grown) abort();
  return grown;
}

// ---------------------------------------------------------------------------
// Strings

// A borrowed view of a NUL-terminated string, for passing to host imports.
// Never return it from a handler (see the ownership rules above).
static inline rift_string_t rift_str(const char *s) {
  rift_string_t out = {(uint8_t *)s, s ? strlen(s) : 0};
  return out;
}

// An owned copy of `len` bytes.
static inline rift_string_t rift_string_dup_n(const char *s, size_t len) {
  rift_string_t out = {NULL, len};
  if (len > 0) {
    out.ptr = (uint8_t *)rift_alloc(len);
    memcpy(out.ptr, s, len);
  }
  return out;
}

// An owned copy of a NUL-terminated string.
static inline rift_string_t rift_string_dup(const char *s) {
  return rift_string_dup_n(s, s ? strlen(s) : 0);
}

// An owned copy of another string.
static inline rift_string_t rift_string_clone(const rift_string_t *s) {
  return rift_string_dup_n((const char *)s->ptr, s->len);
}

// An owned, printf-formatted string. `%.*s` with `(int)s.len, s.ptr` prints a
// `rift_string_t`; `RIFT_STR_ARG(s)` expands to exactly that.
#define RIFT_STR_FMT "%.*s"
#define RIFT_STR_ARG(s) (int)(s).len, (const char *)(s).ptr

static inline rift_string_t rift_string_vfmt(const char *fmt, va_list args) {
  va_list copy;
  va_copy(copy, args);
  int len = vsnprintf(NULL, 0, fmt, copy);
  va_end(copy);
  rift_string_t out = {NULL, 0};
  if (len <= 0) return out;
  char *buf = (char *)rift_alloc((size_t)len + 1);
  vsnprintf(buf, (size_t)len + 1, fmt, args);
  out.ptr = (uint8_t *)buf;
  out.len = (size_t)len;
  return out;
}

static inline rift_string_t rift_string_fmt(const char *fmt, ...)
    __attribute__((format(printf, 1, 2)));
static inline rift_string_t rift_string_fmt(const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  rift_string_t out = rift_string_vfmt(fmt, args);
  va_end(args);
  return out;
}

// A malloc'd NUL-terminated copy; free it with `free()`.
static inline char *rift_string_cstr(const rift_string_t *s) {
  char *out = (char *)rift_alloc(s->len + 1);
  if (s->len) memcpy(out, s->ptr, s->len);
  out[s->len] = '\0';
  return out;
}

static inline bool rift_string_eq(const rift_string_t *s, const char *c) {
  size_t len = strlen(c);
  return s->len == len && (len == 0 || memcmp(s->ptr, c, len) == 0);
}

static inline bool rift_string_eq_string(const rift_string_t *a, const rift_string_t *b) {
  return a->len == b->len && (a->len == 0 || memcmp(a->ptr, b->ptr, a->len) == 0);
}

static inline void rift_string_free(rift_string_t *s) {
  if (s->len > 0) free(s->ptr);
  s->ptr = NULL;
  s->len = 0;
}

static inline rift_option_string_t rift_some(rift_string_t s) {
  rift_option_string_t out;
  out.is_some = true;
  out.val = s;
  return out;
}

static inline rift_option_string_t rift_none(void) {
  rift_option_string_t out;
  memset(&out, 0, sizeof out);
  return out;
}

static inline void rift_option_string_free(rift_option_string_t *s) {
  if (s->is_some) rift_string_free(&s->val);
  s->is_some = false;
}

// ---------------------------------------------------------------------------
// Bytes (bus payloads and store values)

static inline rift_bytes_t rift_bytes_view(const void *data, size_t len) {
  rift_bytes_t out = {(uint8_t *)data, len};
  return out;
}

static inline rift_bytes_t rift_bytes_from_string(const rift_string_t *s) {
  return rift_bytes_view(s->ptr, s->len);
}

static inline void rift_bytes_free(rift_bytes_t *b) {
  if (b->len > 0) free(b->ptr);
  b->ptr = NULL;
  b->len = 0;
}

static inline void rift_string_list_free(rift_string_list_t *list) {
  rift_plugin_list_string_free(list);
  list->ptr = NULL;
  list->len = 0;
}

// ---------------------------------------------------------------------------
// Handler arguments

static inline void rift_context_free(rift_context_t *ctx) { rift_plugin_types_context_free(ctx); }
static inline void rift_bus_message_free(rift_bus_message_t *msg) { rift_plugin_types_bus_message_free(msg); }

// ---------------------------------------------------------------------------
// Actions (decision hooks write into `ret`)

static inline void rift_action_continue(rift_action_t *ret) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_ACTION_CONTINUE;
}

static inline void rift_action_leave_queue(rift_action_t *ret) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_ACTION_LEAVE_QUEUE;
}

// The payload is copied, so literals and borrowed strings are fine here.
static inline void rift_action_with(rift_action_t *ret, uint8_t tag, const char *text) {
  memset(ret, 0, sizeof *ret);
  ret->tag = tag;
  // Every payload-carrying case is a string at the same offset.
  ret->val.deny = rift_string_dup(text);
}

static inline void rift_action_deny(rift_action_t *ret, const char *reason) {
  rift_action_with(ret, RIFT_PLUGIN_TYPES_ACTION_DENY, reason);
}
static inline void rift_action_server(rift_action_t *ret, const char *server) {
  rift_action_with(ret, RIFT_PLUGIN_TYPES_ACTION_SERVER, server);
}
static inline void rift_action_message(rift_action_t *ret, const char *text) {
  rift_action_with(ret, RIFT_PLUGIN_TYPES_ACTION_MESSAGE, text);
}
static inline void rift_action_queue(rift_action_t *ret, const char *queue) {
  rift_action_with(ret, RIFT_PLUGIN_TYPES_ACTION_QUEUE, queue);
}

// ---------------------------------------------------------------------------
// Registration (what `init` returns)

static inline void rift_registration_init(rift_registration_t *reg) {
  memset(reg, 0, sizeof *reg);
}

static inline void rift_registration_add_hook(rift_registration_t *reg, rift_hook_t hook) {
  reg->hooks.ptr = (rift_hook_t *)rift_realloc(reg->hooks.ptr, (reg->hooks.len + 1) * sizeof(rift_hook_t));
  reg->hooks.ptr[reg->hooks.len++] = hook;
}

// `suggest` is one of the payload-free `RIFT_PLUGIN_TYPES_SUGGEST_KIND_*`
// tags (none, players, servers, dynamic). Use
// `rift_registration_add_command_fixed` for a fixed list.
static inline rift_plugin_types_command_spec_t *rift_registration_add_command(
    rift_registration_t *reg, const char *name, const char *permission, uint8_t suggest) {
  reg->commands.ptr = (rift_plugin_types_command_spec_t *)rift_realloc(
      reg->commands.ptr, (reg->commands.len + 1) * sizeof(rift_plugin_types_command_spec_t));
  rift_plugin_types_command_spec_t *spec = &reg->commands.ptr[reg->commands.len++];
  memset(spec, 0, sizeof *spec);
  spec->name = rift_string_dup(name);
  spec->permission = rift_string_dup(permission ? permission : "");
  spec->suggest.tag = suggest;
  return spec;
}

static inline void rift_registration_add_command_fixed(rift_registration_t *reg, const char *name,
                                                       const char *permission,
                                                       const char *const *choices, size_t count) {
  rift_plugin_types_command_spec_t *spec =
      rift_registration_add_command(reg, name, permission, RIFT_PLUGIN_TYPES_SUGGEST_KIND_FIXED);
  spec->suggest.val.fixed.len = count;
  spec->suggest.val.fixed.ptr = count ? (rift_string_t *)rift_alloc(count * sizeof(rift_string_t)) : NULL;
  for (size_t i = 0; i < count; i++) spec->suggest.val.fixed.ptr[i] = rift_string_dup(choices[i]);
}

// ---------------------------------------------------------------------------
// Host calls

static inline void rift_log(rift_level_t level, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static inline void rift_log(rift_level_t level, const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  rift_string_t text = rift_string_vfmt(fmt, args);
  va_end(args);
  rift_plugin_log_log(level, &text);
  rift_string_free(&text);
}

#define RIFT_TRACE RIFT_PLUGIN_TYPES_LEVEL_TRACE
#define RIFT_DEBUG RIFT_PLUGIN_TYPES_LEVEL_DEBUG
#define RIFT_INFO RIFT_PLUGIN_TYPES_LEVEL_INFO
#define RIFT_WARN RIFT_PLUGIN_TYPES_LEVEL_WARN
#define RIFT_ERROR RIFT_PLUGIN_TYPES_LEVEL_ERROR

// Sends `text` to `target` (a name, UUID or connection id). Returns whether
// the player was online; a host error is logged and reported as false.
static inline bool rift_message(const rift_string_t *target, const char *text) {
  rift_string_t t = *target, msg = rift_str(text), err = {NULL, 0};
  bool online = false;
  if (!rift_plugin_players_message(&t, &msg, &online, &err)) {
    rift_log(RIFT_WARN, "players.message failed: " RIFT_STR_FMT, RIFT_STR_ARG(err));
    rift_string_free(&err);
    return false;
  }
  return online;
}

// ---------------------------------------------------------------------------
// JSON configuration
//
// `init` and `reload` receive the plugin configuration as a JSON object.
// `rift_json_get_string(&config, "key", &out)` validates the whole document
// and reads one string member of the top-level object:
//
// - `RIFT_JSON_OK`: `out` holds an owned, unescaped copy (free it with
//   `rift_string_free`);
// - `RIFT_JSON_MISSING`: the key is absent or `null`;
// - `RIFT_JSON_WRONG_TYPE`: the value is not a string;
// - `RIFT_JSON_INVALID`: the input is not a JSON object.
//
// Nested values are validated and skipped, not searched. For richer
// configuration use a real JSON library (cJSON and jsmn build with wasi-sdk).

typedef enum rift_json_result {
  RIFT_JSON_OK = 0,
  RIFT_JSON_MISSING = 1,
  RIFT_JSON_WRONG_TYPE = 2,
  RIFT_JSON_INVALID = 3,
} rift_json_result_t;

static inline void rift__json_ws(const uint8_t **p, const uint8_t *end) {
  while (*p < end && (**p == ' ' || **p == '\t' || **p == '\n' || **p == '\r')) (*p)++;
}

static inline bool rift__json_hex(const uint8_t *p, const uint8_t *end, uint32_t *out) {
  if (end - p < 4) return false;
  uint32_t v = 0;
  for (int i = 0; i < 4; i++) {
    uint8_t c = p[i];
    v <<= 4;
    if (c >= '0' && c <= '9') v |= (uint32_t)(c - '0');
    else if (c >= 'a' && c <= 'f') v |= (uint32_t)(c - 'a' + 10);
    else if (c >= 'A' && c <= 'F') v |= (uint32_t)(c - 'A' + 10);
    else return false;
  }
  *out = v;
  return true;
}

// Parses a string starting at its opening quote. With `out`, appends the
// unescaped UTF-8 to it; the caller frees `out->ptr` either way.
static inline bool rift__json_string(const uint8_t **p, const uint8_t *end, rift_string_t *out) {
  if (*p >= end || **p != '"') return false;
  (*p)++;
  size_t cap = 0;
  while (*p < end) {
    uint8_t c = *(*p)++;
    uint8_t buf[4];
    size_t n = 1;
    buf[0] = c;
    if (c == '"') return true;
    if (c < 0x20) return false;
    if (c == '\\') {
      if (*p >= end) return false;
      uint8_t e = *(*p)++;
      switch (e) {
        case '"': case '\\': case '/': buf[0] = e; break;
        case 'b': buf[0] = '\b'; break;
        case 'f': buf[0] = '\f'; break;
        case 'n': buf[0] = '\n'; break;
        case 'r': buf[0] = '\r'; break;
        case 't': buf[0] = '\t'; break;
        case 'u': {
          uint32_t cp, lo;
          if (!rift__json_hex(*p, end, &cp)) return false;
          *p += 4;
          if (cp >= 0xD800 && cp <= 0xDBFF) {
            if (end - *p < 6 || (*p)[0] != '\\' || (*p)[1] != 'u' || !rift__json_hex(*p + 2, end, &lo) ||
                lo < 0xDC00 || lo > 0xDFFF)
              return false;
            *p += 6;
            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
          } else if (cp >= 0xDC00 && cp <= 0xDFFF) {
            return false;
          }
          if (cp < 0x80) {
            buf[0] = (uint8_t)cp;
          } else if (cp < 0x800) {
            buf[0] = (uint8_t)(0xC0 | (cp >> 6));
            buf[1] = (uint8_t)(0x80 | (cp & 0x3F));
            n = 2;
          } else if (cp < 0x10000) {
            buf[0] = (uint8_t)(0xE0 | (cp >> 12));
            buf[1] = (uint8_t)(0x80 | ((cp >> 6) & 0x3F));
            buf[2] = (uint8_t)(0x80 | (cp & 0x3F));
            n = 3;
          } else {
            buf[0] = (uint8_t)(0xF0 | (cp >> 18));
            buf[1] = (uint8_t)(0x80 | ((cp >> 12) & 0x3F));
            buf[2] = (uint8_t)(0x80 | ((cp >> 6) & 0x3F));
            buf[3] = (uint8_t)(0x80 | (cp & 0x3F));
            n = 4;
          }
          break;
        }
        default: return false;
      }
    }
    if (out) {
      if (out->len + n > cap) {
        cap = (out->len + n) * 2 + 16;
        out->ptr = (uint8_t *)rift_realloc(out->ptr, cap);
      }
      memcpy(out->ptr + out->len, buf, n);
      out->len += n;
    }
  }
  return false;
}

static inline bool rift__json_literal(const uint8_t **p, const uint8_t *end, const char *word) {
  size_t len = strlen(word);
  if ((size_t)(end - *p) < len || memcmp(*p, word, len) != 0) return false;
  *p += len;
  return true;
}

static inline bool rift__json_digits(const uint8_t **p, const uint8_t *end) {
  const uint8_t *start = *p;
  while (*p < end && **p >= '0' && **p <= '9') (*p)++;
  return *p > start;
}

static inline bool rift__json_number(const uint8_t **p, const uint8_t *end) {
  if (*p < end && **p == '-') (*p)++;
  if (*p < end && **p == '0') {
    (*p)++;
  } else if (!rift__json_digits(p, end)) {
    return false;
  }
  if (*p < end && **p == '.') {
    (*p)++;
    if (!rift__json_digits(p, end)) return false;
  }
  if (*p < end && (**p == 'e' || **p == 'E')) {
    (*p)++;
    if (*p < end && (**p == '+' || **p == '-')) (*p)++;
    if (!rift__json_digits(p, end)) return false;
  }
  return true;
}

// Validates and skips one value of any type.
static inline bool rift__json_skip(const uint8_t **p, const uint8_t *end, int depth) {
  if (depth > 64) return false;
  rift__json_ws(p, end);
  if (*p >= end) return false;
  uint8_t c = **p;
  if (c == '"') return rift__json_string(p, end, NULL);
  if (c == 't') return rift__json_literal(p, end, "true");
  if (c == 'f') return rift__json_literal(p, end, "false");
  if (c == 'n') return rift__json_literal(p, end, "null");
  if (c != '{' && c != '[') return rift__json_number(p, end);
  uint8_t close = c == '{' ? '}' : ']';
  (*p)++;
  rift__json_ws(p, end);
  if (*p < end && **p == close) {
    (*p)++;
    return true;
  }
  for (;;) {
    if (c == '{') {
      rift__json_ws(p, end);
      if (!rift__json_string(p, end, NULL)) return false;
      rift__json_ws(p, end);
      if (*p >= end || **p != ':') return false;
      (*p)++;
    }
    if (!rift__json_skip(p, end, depth + 1)) return false;
    rift__json_ws(p, end);
    if (*p >= end) return false;
    if (**p == ',') {
      (*p)++;
      continue;
    }
    if (**p == close) {
      (*p)++;
      return true;
    }
    return false;
  }
}

static inline rift_json_result_t rift_json_get_string_n(const char *json, size_t len, const char *key,
                                                        rift_string_t *out) {
  const uint8_t *p = (const uint8_t *)json, *end = p + len;
  rift_json_result_t result = RIFT_JSON_MISSING;
  rift_string_t found = {NULL, 0};
  out->ptr = NULL;
  out->len = 0;
  rift__json_ws(&p, end);
  if (p >= end || *p != '{') return RIFT_JSON_INVALID;
  p++;
  rift__json_ws(&p, end);
  if (p < end && *p == '}') {
    p++;
  } else {
    for (;;) {
      rift_string_t name = {NULL, 0};
      rift__json_ws(&p, end);
      bool ok = rift__json_string(&p, end, &name);
      bool match = ok && rift_string_eq(&name, key);
      free(name.ptr);
      if (!ok) goto invalid;
      rift__json_ws(&p, end);
      if (p >= end || *p != ':') goto invalid;
      p++;
      rift__json_ws(&p, end);
      if (match) {
        // A later duplicate wins.
        free(found.ptr);
        found.ptr = NULL;
        found.len = 0;
        if (p < end && *p == '"') {
          if (!rift__json_string(&p, end, &found)) goto invalid;
          result = RIFT_JSON_OK;
        } else if (rift__json_literal(&p, end, "null")) {
          result = RIFT_JSON_MISSING;
        } else {
          if (!rift__json_skip(&p, end, 0)) goto invalid;
          result = RIFT_JSON_WRONG_TYPE;
        }
      } else if (!rift__json_skip(&p, end, 0)) {
        goto invalid;
      }
      rift__json_ws(&p, end);
      if (p < end && *p == ',') {
        p++;
        continue;
      }
      if (p < end && *p == '}') {
        p++;
        break;
      }
      goto invalid;
    }
  }
  rift__json_ws(&p, end);
  if (p != end) goto invalid;
  if (result == RIFT_JSON_OK) {
    if (found.len == 0) {
      free(found.ptr);
      found.ptr = NULL;
    }
    *out = found;
  } else {
    free(found.ptr);
  }
  return result;
invalid:
  free(found.ptr);
  return RIFT_JSON_INVALID;
}

static inline rift_json_result_t rift_json_get_string(const rift_string_t *json, const char *key,
                                                      rift_string_t *out) {
  return rift_json_get_string_n((const char *)json->ptr, json->len, key, out);
}

#ifdef __cplusplus
}
#endif

#endif  // RIFT_H
