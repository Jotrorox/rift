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
//   must therefore be heap-allocated: use `rift_string_dup`, `rift_string_fmt`,
//   `rift_text_dup` and the `rift_decision_*`, `rift_suggestions_*` and
//   `rift_registration_*` helpers, never `rift_str("literal")`.
// - Arguments you pass to host imports are borrowed: `rift_str("literal")` and
//   `rift_text_plain("literal")` are fine there and nothing is freed.
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
typedef rift_plugin_types_text_t rift_text_t;
typedef rift_plugin_types_option_text_t rift_option_text_t;
typedef rift_plugin_types_context_t rift_context_t;
typedef rift_plugin_types_decision_t rift_decision_t;
typedef rift_plugin_types_registration_t rift_registration_t;
typedef rift_plugin_types_command_spec_t rift_command_spec_t;
typedef rift_plugin_types_bus_message_t rift_bus_message_t;
typedef rift_plugin_types_hook_t rift_hook_t;
typedef rift_plugin_types_level_t rift_level_t;
typedef rift_plugin_types_dialog_response_t rift_dialog_response_t;
typedef rift_plugin_types_chat_attempt_t rift_chat_attempt_t;
typedef rift_plugin_types_chat_kind_t rift_chat_kind_t;
typedef rift_plugin_types_suggestion_t rift_suggestion_t;
typedef exports_rift_plugin_handler_list_suggestion_t rift_suggestion_list_t;
typedef rift_plugin_types_status_override_t rift_status_override_t;
typedef rift_plugin_types_title_spec_t rift_title_spec_t;
typedef rift_plugin_types_player_info_t rift_player_info_t;
typedef rift_plugin_types_player_record_t rift_player_record_t;
typedef rift_plugin_players_list_player_record_t rift_player_record_list_t;
typedef exports_rift_plugin_handler_result_list_u8_string_t rift_job_outcome_t;

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

// An owned copy of `len` bytes, such as the result `work` returns.
static inline rift_bytes_t rift_bytes_dup(const void *data, size_t len) {
  rift_bytes_t out = {NULL, len};
  if (len > 0) {
    out.ptr = (uint8_t *)rift_alloc(len);
    memcpy(out.ptr, data, len);
  }
  return out;
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
static inline void rift_dialog_response_free(rift_dialog_response_t *response) {
  rift_plugin_types_dialog_response_free(response);
}
static inline void rift_chat_attempt_free(rift_chat_attempt_t *attempt) {
  rift_plugin_types_chat_attempt_free(attempt);
}
static inline void rift_job_outcome_free(rift_job_outcome_t *outcome) {
  exports_rift_plugin_handler_result_list_u8_string_free(outcome);
}

// ---------------------------------------------------------------------------
// Text
//
// Everything a player reads is a `text`: plain (1..=1024 bytes, shown exactly
// as written) or a JSON text component (at most 32 KiB, for colours, line
// breaks, hover text and click events; see the text builders below and
// docs/text.md).
//
// `rift_text_plain` and `rift_text_json` are borrowed views, like `rift_str`.
// Pass them to host imports and to the helpers in this header, which copy
// what they keep. Never store one in a value you return from a handler: use
// `rift_text_dup` or `rift_text_plain_fmt` there.

static inline rift_text_t rift_text_plain_n(const char *s, size_t len) {
  rift_text_t out;
  memset(&out, 0, sizeof out);
  out.tag = RIFT_PLUGIN_TYPES_TEXT_PLAIN;
  out.val.plain.ptr = (uint8_t *)s;
  out.val.plain.len = len;
  return out;
}

static inline rift_text_t rift_text_json_n(const char *json, size_t len) {
  rift_text_t out;
  memset(&out, 0, sizeof out);
  out.tag = RIFT_PLUGIN_TYPES_TEXT_JSON;
  out.val.json.ptr = (uint8_t *)json;
  out.val.json.len = len;
  return out;
}

// A view of a NUL-terminated string as plain text.
static inline rift_text_t rift_text_plain(const char *s) { return rift_text_plain_n(s, s ? strlen(s) : 0); }

// A view of a NUL-terminated JSON text component, such as a text builder
// result.
static inline rift_text_t rift_text_json(const char *json) {
  return rift_text_json_n(json, json ? strlen(json) : 0);
}

// A view of a `rift_string_t`, such as `ctx->name`, as plain text.
static inline rift_text_t rift_text_plain_string(const rift_string_t *s) {
  return rift_text_plain_n((const char *)s->ptr, s->len);
}

// An owned copy of `text`, for values a handler returns.
static inline rift_text_t rift_text_dup(rift_text_t text) {
  rift_text_t out;
  memset(&out, 0, sizeof out);
  out.tag = text.tag;
  // Both cases are a string at the same offset.
  out.val.plain = rift_string_dup_n((const char *)text.val.plain.ptr, text.val.plain.len);
  return out;
}

// Owned, printf-formatted plain text.
static inline rift_text_t rift_text_plain_fmt(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static inline rift_text_t rift_text_plain_fmt(const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  rift_text_t out;
  memset(&out, 0, sizeof out);
  out.tag = RIFT_PLUGIN_TYPES_TEXT_PLAIN;
  out.val.plain = rift_string_vfmt(fmt, args);
  va_end(args);
  return out;
}

static inline void rift_text_free(rift_text_t *text) {
  rift_string_free(&text->val.plain);
}

static inline rift_option_text_t rift_some_text(rift_text_t text) {
  rift_option_text_t out;
  out.is_some = true;
  out.val = text;
  return out;
}

static inline rift_option_text_t rift_none_text(void) {
  rift_option_text_t out;
  memset(&out, 0, sizeof out);
  return out;
}

// ---------------------------------------------------------------------------
// Decisions (decision hooks write into `ret`)
//
// `login`, `before_transfer` and `chat` accept continue and deny;
// `initial_server` and `kick` also server; `command` accepts continue,
// message, server, queue and leave-queue. Every payload is copied, so
// literals and borrowed views are fine here.

static inline void rift_decision_continue(rift_decision_t *ret) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_CONTINUE;
}

static inline void rift_decision_leave_queue(rift_decision_t *ret) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_LEAVE_QUEUE;
}

// `rift_decision_deny(ret, rift_text_plain("You are banned."))`, or
// `rift_text_json(json)` for a JSON text component.
static inline void rift_decision_deny(rift_decision_t *ret, rift_text_t reason) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_DENY;
  ret->val.deny = rift_text_dup(reason);
}

static inline void rift_decision_message(rift_decision_t *ret, rift_text_t text) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_MESSAGE;
  ret->val.message = rift_text_dup(text);
}

static inline void rift_decision_server(rift_decision_t *ret, const char *server) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_SERVER;
  ret->val.server = rift_string_dup(server);
}

static inline void rift_decision_queue(rift_decision_t *ret, const char *queue) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_QUEUE;
  ret->val.queue = rift_string_dup(queue);
}

// Deny and message with printf-formatted plain text.
static inline void rift_decision_deny_fmt(rift_decision_t *ret, const char *fmt, ...)
    __attribute__((format(printf, 2, 3)));
static inline void rift_decision_deny_fmt(rift_decision_t *ret, const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_DENY;
  ret->val.deny.tag = RIFT_PLUGIN_TYPES_TEXT_PLAIN;
  ret->val.deny.val.plain = rift_string_vfmt(fmt, args);
  va_end(args);
}

static inline void rift_decision_message_fmt(rift_decision_t *ret, const char *fmt, ...)
    __attribute__((format(printf, 2, 3)));
static inline void rift_decision_message_fmt(rift_decision_t *ret, const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_DECISION_MESSAGE;
  ret->val.message.tag = RIFT_PLUGIN_TYPES_TEXT_PLAIN;
  ret->val.message.val.plain = rift_string_vfmt(fmt, args);
  va_end(args);
}

// Frees a decision built with the helpers above that was not returned from a
// hook or given to `rift_background_resolve`.
static inline void rift_decision_free(rift_decision_t *decision) {
  rift_plugin_types_decision_free(decision);
  rift_decision_continue(decision);
}

// ---------------------------------------------------------------------------
// Chat and kick
//
// `exports_rift_plugin_handler_chat` sees every chat message, and every
// command that reaches the backend, before the backend does. It answers
// `continue`, which lets the packet through unchanged, or `deny`, which drops
// it and shows the player the text. `attempt->kind` tells a message from a
// command; for a command `attempt->ctx.command` is the root name and
// `attempt->ctx.args` the rest, and `attempt->text` is the whole line without
// its slash.
//
// `exports_rift_plugin_handler_kick` runs when a backend kicks a player in
// play (`ctx->server` is the backend, `ctx->reason` its reason). `continue`
// leaves it to `network.on_kick`, `deny` disconnects the player with the
// text, `server` moves them there.
//
// Rift calls either hook only when `init` lists it
// (`RIFT_PLUGIN_TYPES_HOOK_CHAT`, `RIFT_PLUGIN_TYPES_HOOK_KICK`): an empty
// hook list does not imply them. A plugin that lists `chat` is asked about
// every message of every player. Dropping one signed message breaks the
// player's signature chain, so unless the proxy sets
// `authentication.signed_chat = false`, the first deny costs that player
// their chat signing until they reconnect.
//
//   void exports_rift_plugin_handler_chat(rift_chat_attempt_t *attempt, rift_decision_t *ret) {
//     if (attempt->kind == RIFT_PLUGIN_TYPES_CHAT_KIND_MESSAGE && is_muted(&attempt->ctx.uuid)) {
//       rift_decision_deny(ret, rift_text_plain("You are muted."));
//     } else {
//       rift_decision_continue(ret);
//     }
//     rift_chat_attempt_free(attempt);
//   }

// ---------------------------------------------------------------------------
// Text builders
//
// Each builder writes a JSON text component and returns it as a malloc'd,
// NUL-terminated string; wrap it with `rift_text_json` and free it with
// `free()`. Everything written is JSON-escaped.
//
// - `rift_mini` writes Rift markup as `{"mini": "...", "args": {...}}`; Rift
//   turns it into a component (see docs/text.md for the tags).
// - `rift_legacy` writes `&c` codes as `{"legacy": "...", "marker": "&"}`.
// - `rift_msg` names a message from the plugin's `messages.toml` as
//   `{"message": "...", "args": {...}}`; Rift renders it in the player's
//   language. (`rift_message` sends a player text.)
// - `rift_component` builds a JSON text component field by field, with the
//   1.21.5 spellings `click_event` and `hover_event` (Rift rewrites them for
//   older clients).
//
// A bare string is never read as markup, so player input passed through
// `rift_mini_arg` cannot add tags or click events:
//
//   rift_mini_t m = rift_mini("<gold>Welcome, <player>!");
//   rift_mini_arg(&m, "player", name);
//   char *json = rift_mini_finish(&m);
//   rift_message(&target, rift_text_json(json));
//   free(json);

// A growable byte buffer; `ptr` is NUL-terminated whenever it is non-NULL.
typedef struct rift_text_buf {
  char *ptr;
  size_t len, cap;
} rift_text_buf_t;

static inline void rift_text_buf_append_n(rift_text_buf_t *b, const char *s, size_t n) {
  if (b->len + n + 1 > b->cap) {
    b->cap = (b->len + n + 1) * 2;
    b->ptr = (char *)rift_realloc(b->ptr, b->cap);
  }
  if (n) memcpy(b->ptr + b->len, s, n);
  b->len += n;
  b->ptr[b->len] = '\0';
}

static inline void rift_text_buf_append(rift_text_buf_t *b, const char *s) {
  rift_text_buf_append_n(b, s, s ? strlen(s) : 0);
}

// The buffer's text, which the caller frees; the buffer is empty afterwards.
static inline char *rift_text_buf_take(rift_text_buf_t *b) {
  if (!b->ptr) rift_text_buf_append_n(b, "", 0);
  char *out = b->ptr;
  memset(b, 0, sizeof *b);
  return out;
}

// The length of a valid UTF-8 sequence at `s`, or 0.
static inline size_t rift__utf8_len(const uint8_t *s, size_t n) {
  uint8_t c = s[0];
  size_t len;
  uint32_t min, cp;
  if (c >= 0xC2 && c <= 0xDF) len = 2, min = 0x80, cp = c & 0x1F;
  else if (c >= 0xE0 && c <= 0xEF) len = 3, min = 0x800, cp = c & 0x0F;
  else if (c >= 0xF0 && c <= 0xF4) len = 4, min = 0x10000, cp = c & 0x07;
  else return 0;
  if (n < len) return 0;
  for (size_t i = 1; i < len; i++) {
    if ((s[i] & 0xC0) != 0x80) return 0;
    cp = (cp << 6) | (s[i] & 0x3F);
  }
  if (cp < min || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) return 0;
  return len;
}

// Appends `len` bytes of `s` as a JSON string. Invalid UTF-8 becomes U+FFFD.
static inline void rift_text_buf_quote_n(rift_text_buf_t *b, const char *s, size_t len) {
  static const char hex[] = "0123456789abcdef";
  const uint8_t *p = (const uint8_t *)s;
  rift_text_buf_append_n(b, "\"", 1);
  size_t i = 0;
  while (i < len) {
    uint8_t c = p[i];
    if (c < 0x80) {
      char esc[6] = {'\\', 'u', '0', '0', hex[c >> 4], hex[c & 0xF]};
      switch (c) {
        case '"': rift_text_buf_append_n(b, "\\\"", 2); break;
        case '\\': rift_text_buf_append_n(b, "\\\\", 2); break;
        case '\n': rift_text_buf_append_n(b, "\\n", 2); break;
        case '\r': rift_text_buf_append_n(b, "\\r", 2); break;
        case '\t': rift_text_buf_append_n(b, "\\t", 2); break;
        default:
          if (c < 0x20) rift_text_buf_append_n(b, esc, 6);
          else rift_text_buf_append_n(b, (const char *)&p[i], 1);
      }
      i++;
      continue;
    }
    size_t n = rift__utf8_len(p + i, len - i);
    if (n == 0) {
      rift_text_buf_append_n(b, "\\ufffd", 6);
      i++;
    } else {
      rift_text_buf_append_n(b, (const char *)&p[i], n);
      i += n;
    }
  }
  rift_text_buf_append_n(b, "\"", 1);
}

static inline void rift_text_buf_quote(rift_text_buf_t *b, const char *s) {
  rift_text_buf_quote_n(b, s, s ? strlen(s) : 0);
}

// `s` as an owned JSON string, for writing JSON by hand.
static inline rift_string_t rift_string_quote(const rift_string_t *s) {
  rift_text_buf_t b = {NULL, 0, 0};
  rift_text_buf_quote_n(&b, (const char *)s->ptr, s->len);
  rift_string_t out = {(uint8_t *)b.ptr, b.len};
  return out;
}

// Markup with named arguments. Start with `rift_mini`, add arguments, then
// `rift_mini_finish` returns the JSON and releases the builder.
typedef struct rift_mini {
  rift_text_buf_t markup;
  rift_text_buf_t args;
} rift_mini_t;

static inline rift_mini_t rift_mini(const char *markup) {
  rift_mini_t m;
  memset(&m, 0, sizeof m);
  rift_text_buf_quote(&m.markup, markup);
  return m;
}

static inline void rift__args_key(rift_text_buf_t *args, const char *name) {
  rift_text_buf_append(args, args->len ? "," : "{");
  rift_text_buf_quote(args, name);
  rift_text_buf_append_n(args, ":", 1);
}

static inline void rift__mini_key(rift_mini_t *m, const char *name) { rift__args_key(&m->args, name); }

// Appends the value of a style argument: `{"style": markup}`.
static inline void rift__args_style(rift_text_buf_t *args, const char *markup) {
  rift_text_buf_append(args, "{\"style\":");
  rift_text_buf_quote(args, markup);
  rift_text_buf_append_n(args, "}", 1);
}

// `<name>` shows `text` exactly as written: it is never parsed, so a player
// name cannot add tags. Give each name once.
static inline void rift_mini_arg(rift_mini_t *m, const char *name, const char *text) {
  rift__mini_key(m, name);
  rift_text_buf_quote(&m->args, text);
}

// `<name>` shows a component with its own formatting, such as the result of
// `rift_component_finish` or `rift_legacy`, inserted as JSON.
static inline void rift_mini_arg_component(rift_mini_t *m, const char *name, const char *component_json) {
  rift__mini_key(m, name);
  rift_text_buf_append(&m->args, component_json);
}

// `<name>` is a style: markup made only of formatting tags, such as
// "<#55ffff><bold>". It wraps whatever the template puts inside
// `<name>...</name>`.
static inline void rift_mini_arg_style(rift_mini_t *m, const char *name, const char *markup) {
  rift__mini_key(m, name);
  rift__args_style(&m->args, markup);
}

// `{"<kind>": <quoted>, "args": {...}}`; frees both buffers.
static inline char *rift__args_finish(const char *head, rift_text_buf_t *quoted, rift_text_buf_t *args) {
  rift_text_buf_t out = {NULL, 0, 0};
  rift_text_buf_append(&out, head);
  rift_text_buf_append_n(&out, quoted->ptr, quoted->len);
  if (args->len) {
    rift_text_buf_append(&out, ",\"args\":");
    rift_text_buf_append_n(&out, args->ptr, args->len);
    rift_text_buf_append_n(&out, "}", 1);
  }
  rift_text_buf_append_n(&out, "}", 1);
  free(quoted->ptr);
  free(args->ptr);
  memset(quoted, 0, sizeof *quoted);
  memset(args, 0, sizeof *args);
  return rift_text_buf_take(&out);
}

static inline char *rift_mini_finish(rift_mini_t *m) { return rift__args_finish("{\"mini\":", &m->markup, &m->args); }

// A message from the plugin's `messages.toml`, by key. A key with `:` names
// another catalog, such as "party:invited" or "rift:queue.full". Rift renders
// it in the recipient's language and checks the key and the arguments at the
// call. Start with `rift_msg`, add arguments, then `rift_msg_finish` returns
// the JSON and releases the builder:
//
//   rift_msg_t m = rift_msg("invited");
//   rift_msg_arg(&m, "from", name);
//   char *json = rift_msg_finish(&m);
//   rift_message(&target, rift_text_json(json));
//   free(json);
typedef struct rift_msg {
  rift_text_buf_t key;
  rift_text_buf_t args;
} rift_msg_t;

static inline rift_msg_t rift_msg(const char *key) {
  rift_msg_t m;
  memset(&m, 0, sizeof m);
  rift_text_buf_quote(&m.key, key);
  return m;
}

// `name` shows `text` exactly as written: it is never parsed. Give each name
// once.
static inline void rift_msg_arg(rift_msg_t *m, const char *name, const char *text) {
  rift__args_key(&m->args, name);
  rift_text_buf_quote(&m->args, text);
}

// `name` shows a component with its own formatting, inserted as JSON.
static inline void rift_msg_arg_component(rift_msg_t *m, const char *name, const char *component_json) {
  rift__args_key(&m->args, name);
  rift_text_buf_append(&m->args, component_json);
}

// `name` is a style argument (see `rift_mini_arg_style`); the catalog
// declares it under `styles`.
static inline void rift_msg_arg_style(rift_msg_t *m, const char *name, const char *markup) {
  rift__args_key(&m->args, name);
  rift__args_style(&m->args, markup);
}

static inline char *rift_msg_finish(rift_msg_t *m) { return rift__args_finish("{\"message\":", &m->key, &m->args); }

// `text` escaped so it can be spliced into markup: every `\` becomes `\\` and
// every `<` becomes `\<`. Nothing else changes. Returns a malloc'd string;
// free it with `free()`. NULL is treated as "". An argument
// (`rift_mini_arg`) is never parsed and needs no escaping.
static inline char *rift_text_escape(const char *text) {
  rift_text_buf_t out = {NULL, 0, 0};
  const char *run = text ? text : "";
  for (const char *p = run; *p; p++) {
    if (*p == '<' || *p == '\\') {
      rift_text_buf_append_n(&out, run, (size_t)(p - run));
      rift_text_buf_append_n(&out, "\\", 1);
      run = p;
    }
  }
  rift_text_buf_append(&out, run);
  return rift_text_buf_take(&out);
}

// Legacy codes with `marker`, a one-symbol UTF-8 string such as "&" or "§":
// `&cRed &lbold`, and `&#rrggbb` for a hex colour.
static inline char *rift_legacy_marker(const char *text, const char *marker) {
  rift_text_buf_t out = {NULL, 0, 0};
  rift_text_buf_append(&out, "{\"legacy\":");
  rift_text_buf_quote(&out, text);
  rift_text_buf_append(&out, ",\"marker\":");
  rift_text_buf_quote(&out, marker);
  rift_text_buf_append_n(&out, "}", 1);
  return rift_text_buf_take(&out);
}

// Legacy codes with the `&` marker.
static inline char *rift_legacy(const char *text) { return rift_legacy_marker(text, "&"); }

// A JSON text component. Start with `rift_component` (or `rift_translate`,
// `rift_keybind`, `rift_sprite`, `rift_head`), call the setters (each at most
// once), then `rift_component_finish` returns the JSON and releases the
// builder:
//
//   rift_component_t c = rift_component("[Accept]");
//   rift_component_color(&c, "green");
//   rift_component_bold(&c);
//   rift_component_run_command(&c, "/party accept Alex");
//   rift_component_hover_text(&c, "Join Alex's party");
//   char *json = rift_component_finish(&c);
typedef struct rift_component {
  rift_text_buf_t fields;
  rift_text_buf_t with;
  rift_text_buf_t extra;
} rift_component_t;

// A component whose first field is `key` with `value` quoted.
static inline rift_component_t rift__component_start(const char *key, const char *value) {
  rift_component_t c;
  memset(&c, 0, sizeof c);
  rift_text_buf_append_n(&c.fields, "{", 1);
  rift_text_buf_quote(&c.fields, key);
  rift_text_buf_append_n(&c.fields, ":", 1);
  rift_text_buf_quote(&c.fields, value);
  return c;
}

// A component showing `text`.
static inline rift_component_t rift_component(const char *text) { return rift__component_start("text", text); }

// A translation the client looks up by `key`, such as
// "multiplayer.player.joined". Add arguments with `rift_component_with` and a
// fallback with `rift_component_fallback`.
static inline rift_component_t rift_translate(const char *key) { return rift__component_start("translate", key); }

// The key the player bound to `key`, such as "key.jump".
static inline rift_component_t rift_keybind(const char *key) { return rift__component_start("keybind", key); }

// A sprite from the blocks atlas, such as "item/diamond". Choose another atlas
// with `rift_component_atlas`. Clients before 1.21.9 do not show it.
static inline rift_component_t rift_sprite(const char *sprite) {
  rift_component_t c = rift__component_start("object", "atlas");
  rift_text_buf_append(&c.fields, ",\"sprite\":");
  rift_text_buf_quote(&c.fields, sprite);
  return c;
}

// The head of the player called `name`. Clients before 1.21.9 do not show it.
static inline rift_component_t rift_head(const char *name) {
  rift_component_t c = rift__component_start("object", "player");
  rift_text_buf_append(&c.fields, ",\"player\":{\"name\":");
  rift_text_buf_quote(&c.fields, name);
  rift_text_buf_append_n(&c.fields, "}", 1);
  return c;
}

// Any other field as raw JSON, such as `rift_component_field(&c, "font",
// "\"minecraft:uniform\"")` or `rift_component_field(&c, "italic", "false")`.
static inline void rift_component_field(rift_component_t *c, const char *key, const char *raw_json) {
  rift_text_buf_append_n(&c->fields, ",", 1);
  rift_text_buf_quote(&c->fields, key);
  rift_text_buf_append_n(&c->fields, ":", 1);
  rift_text_buf_append(&c->fields, raw_json);
}

static inline void rift__component_string(rift_component_t *c, const char *key, const char *value) {
  rift_text_buf_append_n(&c->fields, ",", 1);
  rift_text_buf_quote(&c->fields, key);
  rift_text_buf_append_n(&c->fields, ":", 1);
  rift_text_buf_quote(&c->fields, value);
}

// A colour name such as "red" or "#ff8800".
static inline void rift_component_color(rift_component_t *c, const char *color) {
  rift__component_string(c, "color", color);
}
static inline void rift_component_bold(rift_component_t *c) { rift_component_field(c, "bold", "true"); }
static inline void rift_component_italic(rift_component_t *c) { rift_component_field(c, "italic", "true"); }
static inline void rift_component_underlined(rift_component_t *c) { rift_component_field(c, "underlined", "true"); }
static inline void rift_component_strikethrough(rift_component_t *c) {
  rift_component_field(c, "strikethrough", "true");
}
static inline void rift_component_obfuscated(rift_component_t *c) { rift_component_field(c, "obfuscated", "true"); }

// Turns a decoration ("bold", "italic", "underlined", "strikethrough" or
// "obfuscated") on or off. Off overrides what a parent component sets:
// `rift_component_decoration(&c, "italic", false)`. Use it instead of
// `rift_component_bold` and the others, not as well.
static inline void rift_component_decoration(rift_component_t *c, const char *name, bool on) {
  rift_component_field(c, name, on ? "true" : "false");
}

// The font, such as "minecraft:uniform".
static inline void rift_component_font(rift_component_t *c, const char *font) {
  rift__component_string(c, "font", font);
}

// The ARGB integer vanilla stores for a shadow, with the same arithmetic as
// the host's `<shadow>` tag; false when `color` is not "#rrggbb".
static inline bool rift__shadow_color(const char *color, double opacity, int32_t *argb) {
  if (!color || color[0] != '#' || strlen(color) != 7) return false;
  uint32_t rgb = 0;
  for (int i = 1; i < 7; i++) {
    char d = color[i];
    uint32_t v;
    if (d >= '0' && d <= '9') v = (uint32_t)(d - '0');
    else if (d >= 'a' && d <= 'f') v = (uint32_t)(d - 'a' + 10);
    else if (d >= 'A' && d <= 'F') v = (uint32_t)(d - 'A' + 10);
    else return false;
    rgb = (rgb << 4) | v;
  }
  if (!(opacity >= 0.0)) opacity = 0.0;  // also NaN
  if (opacity > 1.0) opacity = 1.0;
  uint32_t alpha = (uint32_t)(opacity * 255.0 + 0.5);
  uint32_t bits = (alpha << 24) | rgb;
  // Vanilla stores ARGB as a signed int.
  memcpy(argb, &bits, sizeof bits);
  return true;
}

// A text shadow in `color` ("#rrggbb") with `opacity` from 0 to 1, as
// `<shadow:#rrggbb:opacity>` writes it. An opacity of 0 hides the shadow. A
// color that is not "#rrggbb" makes Rift reject the component at the call.
static inline void rift_component_shadow(rift_component_t *c, const char *color, double opacity) {
  int32_t argb;
  if (!rift__shadow_color(color, opacity, &argb)) {
    rift__component_string(c, "shadow_color", color);
    return;
  }
  char num[16];
  snprintf(num, sizeof num, "%ld", (long)argb);
  rift_component_field(c, "shadow_color", num);
}

// The atlas of a `rift_sprite`, such as "minecraft:gui".
static inline void rift_component_atlas(rift_component_t *c, const char *atlas) {
  rift__component_string(c, "atlas", atlas);
}

// Whether a `rift_head` shows the hat layer (it does unless set to false).
static inline void rift_component_hat(rift_component_t *c, bool shown) {
  rift_component_field(c, "hat", shown ? "true" : "false");
}

// The text a client shows when it has no translation for the key of a
// `rift_translate` component.
static inline void rift_component_fallback(rift_component_t *c, const char *text) {
  rift__component_string(c, "fallback", text);
}

// Appends an argument of a `rift_translate` component, given as JSON (a
// finished component or an authoring object).
static inline void rift_component_with(rift_component_t *c, const char *arg_json) {
  rift_text_buf_append_n(&c->with, c->with.len ? "," : "[", 1);
  rift_text_buf_append(&c->with, arg_json);
}

// Appends an argument of a `rift_translate` component shown as written.
static inline void rift_component_with_text(rift_component_t *c, const char *text) {
  rift_text_buf_append_n(&c->with, c->with.len ? "," : "[", 1);
  rift_text_buf_quote(&c->with, text);
}

static inline void rift__component_click(rift_component_t *c, const char *action, const char *key,
                                         const char *value) {
  rift_text_buf_append(&c->fields, ",\"click_event\":{\"action\":");
  rift_text_buf_quote(&c->fields, action);
  rift_text_buf_append_n(&c->fields, ",", 1);
  rift_text_buf_quote(&c->fields, key);
  rift_text_buf_append_n(&c->fields, ":", 1);
  rift_text_buf_quote(&c->fields, value);
  rift_text_buf_append_n(&c->fields, "}", 1);
}

// A click runs `command` (with its leading `/`) as the player.
static inline void rift_component_run_command(rift_component_t *c, const char *command) {
  rift__component_click(c, "run_command", "command", command);
}
// A click puts `command` into the player's chat box.
static inline void rift_component_suggest_command(rift_component_t *c, const char *command) {
  rift__component_click(c, "suggest_command", "command", command);
}
// A click opens `url` (http or https) after a confirmation.
static inline void rift_component_open_url(rift_component_t *c, const char *url) {
  rift__component_click(c, "open_url", "url", url);
}
// A click copies `text` to the clipboard.
static inline void rift_component_copy_to_clipboard(rift_component_t *c, const char *text) {
  rift__component_click(c, "copy_to_clipboard", "value", text);
}
// A click turns a book to `page`, counting from 1.
static inline void rift_component_change_page(rift_component_t *c, int page) {
  char num[16];
  snprintf(num, sizeof num, "%d", page);
  rift_text_buf_append(&c->fields, ",\"click_event\":{\"action\":\"change_page\",\"page\":");
  rift_text_buf_append(&c->fields, num);
  rift_text_buf_append_n(&c->fields, "}", 1);
}

// Hover text as a component given as JSON, such as another finished
// component or a `rift_mini` result.
static inline void rift_component_hover(rift_component_t *c, const char *component_json) {
  rift_text_buf_append(&c->fields, ",\"hover_event\":{\"action\":\"show_text\",\"value\":");
  rift_text_buf_append(&c->fields, component_json);
  rift_text_buf_append_n(&c->fields, "}", 1);
}

// Hover text shown as written.
static inline void rift_component_hover_text(rift_component_t *c, const char *text) {
  rift_text_buf_append(&c->fields, ",\"hover_event\":{\"action\":\"show_text\",\"value\":");
  rift_text_buf_quote(&c->fields, text);
  rift_text_buf_append_n(&c->fields, "}", 1);
}

// Hover shows `count` of the item `id`, such as "minecraft:diamond".
static inline void rift_component_hover_item(rift_component_t *c, const char *id, int count) {
  char num[16];
  snprintf(num, sizeof num, "%d", count);
  rift_text_buf_append(&c->fields, ",\"hover_event\":{\"action\":\"show_item\",\"id\":");
  rift_text_buf_quote(&c->fields, id);
  rift_text_buf_append(&c->fields, ",\"count\":");
  rift_text_buf_append(&c->fields, num);
  rift_text_buf_append_n(&c->fields, "}", 1);
}

// Hover shows the entity of type `kind`, such as "minecraft:pig", with `uuid`
// in its hyphenated form. `name_json` (a finished component or an authoring
// object) may be NULL for none.
static inline void rift_component_hover_entity(rift_component_t *c, const char *kind, const char *uuid,
                                               const char *name_json) {
  rift_text_buf_append(&c->fields, ",\"hover_event\":{\"action\":\"show_entity\",\"id\":");
  rift_text_buf_quote(&c->fields, kind);
  rift_text_buf_append(&c->fields, ",\"uuid\":");
  rift_text_buf_quote(&c->fields, uuid);
  if (name_json) {
    rift_text_buf_append(&c->fields, ",\"name\":");
    rift_text_buf_append(&c->fields, name_json);
  }
  rift_text_buf_append_n(&c->fields, "}", 1);
}

// Text inserted into the chat box on shift-click.
static inline void rift_component_insertion(rift_component_t *c, const char *text) {
  rift__component_string(c, "insertion", text);
}

// Appends a child given as JSON (a finished component or an authoring
// object). Children inherit this component's style.
static inline void rift_component_extra(rift_component_t *c, const char *child_json) {
  rift_text_buf_append_n(&c->extra, c->extra.len ? "," : "[", 1);
  rift_text_buf_append(&c->extra, child_json);
}

// Appends a child shown as written.
static inline void rift_component_extra_text(rift_component_t *c, const char *text) {
  rift_text_buf_append_n(&c->extra, c->extra.len ? "," : "[", 1);
  rift_text_buf_quote(&c->extra, text);
}

static inline char *rift_component_finish(rift_component_t *c) {
  if (c->with.len) {
    rift_text_buf_append(&c->fields, ",\"with\":");
    rift_text_buf_append_n(&c->fields, c->with.ptr, c->with.len);
    rift_text_buf_append_n(&c->fields, "]", 1);
  }
  if (c->extra.len) {
    rift_text_buf_append(&c->fields, ",\"extra\":");
    rift_text_buf_append_n(&c->fields, c->extra.ptr, c->extra.len);
    rift_text_buf_append_n(&c->fields, "]", 1);
  }
  rift_text_buf_append_n(&c->fields, "}", 1);
  free(c->with.ptr);
  free(c->extra.ptr);
  char *out = rift_text_buf_take(&c->fields);
  memset(c, 0, sizeof *c);
  return out;
}

// ---------------------------------------------------------------------------
// Suggestions and server-list text (what `suggest` and `status` return)

// Appends a suggestion; `tooltip` (copied) may be NULL for none:
//
//   rift_text_t tip = rift_text_plain("The main lobby");
//   rift_suggestions_add(ret, "lobby", &tip);
static inline void rift_suggestions_add(rift_suggestion_list_t *list, const char *text, const rift_text_t *tooltip) {
  list->ptr = (rift_suggestion_t *)rift_realloc(list->ptr, (list->len + 1) * sizeof(rift_suggestion_t));
  rift_suggestion_t *s = &list->ptr[list->len++];
  memset(s, 0, sizeof *s);
  s->text = rift_string_dup(text);
  s->tooltip = tooltip ? rift_some_text(rift_text_dup(*tooltip)) : rift_none_text();
}

// An override that replaces nothing. `status` returns true to apply it; set
// the fields to owned values, such as
// `ret->motd = rift_some_text(rift_text_dup(rift_text_json(json)))`.
static inline void rift_status_override_init(rift_status_override_t *ret) { memset(ret, 0, sizeof *ret); }

// Appends a sample line (copied); clients show it as `§` codes.
static inline void rift_status_override_add_sample(rift_status_override_t *ret, rift_text_t line) {
  rift_plugin_types_list_text_t *list = &ret->sample.val;
  ret->sample.is_some = true;
  list->ptr = (rift_text_t *)rift_realloc(list->ptr, (list->len + 1) * sizeof(rift_text_t));
  list->ptr[list->len++] = rift_text_dup(line);
}

// ---------------------------------------------------------------------------
// Registration (what `init` returns)

static inline void rift_registration_init(rift_registration_t *reg) {
  memset(reg, 0, sizeof *reg);
}

// An empty hook list delivers every hook except `chat` and `kick`; those two
// are delivered only when listed.
static inline void rift_registration_add_hook(rift_registration_t *reg, rift_hook_t hook) {
  reg->hooks.ptr = (rift_hook_t *)rift_realloc(reg->hooks.ptr, (reg->hooks.len + 1) * sizeof(rift_hook_t));
  reg->hooks.ptr[reg->hooks.len++] = hook;
}

// `suggest` is one of the payload-free `RIFT_PLUGIN_TYPES_SUGGEST_KIND_*`
// tags (none, players, servers, dynamic). Use
// `rift_registration_add_command_fixed` for a fixed list. `permission` is the
// node a player needs to see and run the command; `""` or NULL opens it to
// every player. The returned spec stays valid until the next command is
// added: use it right away with `rift_command_add_alias` and
// `rift_command_set_console`.
static inline rift_command_spec_t *rift_registration_add_command(rift_registration_t *reg, const char *name,
                                                                 const char *permission, uint8_t suggest) {
  reg->commands.ptr =
      (rift_command_spec_t *)rift_realloc(reg->commands.ptr, (reg->commands.len + 1) * sizeof(rift_command_spec_t));
  rift_command_spec_t *spec = &reg->commands.ptr[reg->commands.len++];
  memset(spec, 0, sizeof *spec);
  spec->name = rift_string_dup(name);
  spec->permission = rift_string_dup(permission ? permission : "");
  spec->suggest.tag = suggest;
  return spec;
}

static inline rift_command_spec_t *rift_registration_add_command_fixed(rift_registration_t *reg, const char *name,
                                                                       const char *permission,
                                                                       const char *const *choices, size_t count) {
  rift_command_spec_t *spec =
      rift_registration_add_command(reg, name, permission, RIFT_PLUGIN_TYPES_SUGGEST_KIND_FIXED);
  spec->suggest.val.fixed.len = count;
  spec->suggest.val.fixed.ptr = count ? (rift_string_t *)rift_alloc(count * sizeof(rift_string_t)) : NULL;
  for (size_t i = 0; i < count; i++) spec->suggest.val.fixed.ptr[i] = rift_string_dup(choices[i]);
  return spec;
}

// Another name for the command (at most 16). The hooks always see the
// command's own name in `ctx->command`.
static inline void rift_command_add_alias(rift_command_spec_t *spec, const char *alias) {
  spec->aliases.ptr =
      (rift_string_t *)rift_realloc(spec->aliases.ptr, (spec->aliases.len + 1) * sizeof(rift_string_t));
  spec->aliases.ptr[spec->aliases.len++] = rift_string_dup(alias);
}

// Whether operators may run the command from the console (`rift admin
// plugins run`). The hook then sees `ctx->console`, with `ctx->name`
// `CONSOLE`, an empty `ctx->uuid` and connection id 0.
static inline void rift_command_set_console(rift_command_spec_t *spec, bool console) { spec->console = console; }

// ---------------------------------------------------------------------------
// Host calls
//
// The wrappers below log a host error at warn level and report it as false.
// `target` is a name, a UUID or a connection id, such as `&ctx->uuid`. Every
// other host function is the generated `rift_plugin_<interface>_<function>`.

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

static inline void rift__host_error(const char *function, rift_string_t *err) {
  rift_log(RIFT_WARN, "%s failed: " RIFT_STR_FMT, function, RIFT_STR_ARG(*err));
  rift_string_free(err);
}

// Sends `text` to `target`. Returns whether the player was online.
static inline bool rift_message(const rift_string_t *target, rift_text_t text) {
  rift_string_t t = *target, err = {NULL, 0};
  bool online = false;
  if (!rift_plugin_players_message(&t, &text, &online, &err)) {
    rift__host_error("players.message", &err);
    return false;
  }
  return online;
}

// Shows `text` above `target`'s hotbar. Returns whether the player was online.
static inline bool rift_action_bar(const rift_string_t *target, rift_text_t text) {
  rift_string_t t = *target, err = {NULL, 0};
  bool online = false;
  if (!rift_plugin_players_action_bar(&t, &text, &online, &err)) {
    rift__host_error("players.action-bar", &err);
    return false;
  }
  return online;
}

// Disconnects `target` with `reason`. Returns whether the player was online.
static inline bool rift_kick(const rift_string_t *target, rift_text_t reason) {
  rift_string_t t = *target, err = {NULL, 0};
  bool online = false;
  if (!rift_plugin_players_kick(&t, &reason, &online, &err)) {
    rift__host_error("players.kick", &err);
    return false;
  }
  return online;
}

// Sends `text` to every player. Returns how many received it.
static inline uint32_t rift_broadcast(rift_text_t text) {
  rift_string_t err = {NULL, 0};
  uint32_t delivered = 0;
  if (!rift_plugin_players_broadcast(&text, &delivered, &err)) {
    rift__host_error("players.broadcast", &err);
    return 0;
  }
  return delivered;
}

// A title spec that borrows its texts, for `rift_plugin_players_title`.
// `title` and `subtitle` may be NULL, but not both; without `title` only the
// subtitle is shown. Times are ticks (vanilla: 10, 70, 20).
static inline rift_title_spec_t rift_title_spec(const rift_text_t *title, const rift_text_t *subtitle,
                                                int32_t fade_in, int32_t stay, int32_t fade_out) {
  rift_title_spec_t spec;
  memset(&spec, 0, sizeof spec);
  if (title) spec.title = rift_some_text(*title);
  if (subtitle) spec.subtitle = rift_some_text(*subtitle);
  spec.fade_in = fade_in;
  spec.stay = stay;
  spec.fade_out = fade_out;
  return spec;
}

// Shows `target` a title; the arguments are those of `rift_title_spec`.
// Returns whether the player was online.
static inline bool rift_title(const rift_string_t *target, const rift_text_t *title, const rift_text_t *subtitle,
                              int32_t fade_in, int32_t stay, int32_t fade_out) {
  rift_string_t t = *target, err = {NULL, 0};
  rift_title_spec_t spec = rift_title_spec(title, subtitle, fade_in, stay, fade_out);
  bool online = false;
  if (!rift_plugin_players_title(&t, &spec, &online, &err)) {
    rift__host_error("players.title", &err);
    return false;
  }
  return online;
}

// Shows `target` a dialog given as JSON in the vanilla dialog format
// (Minecraft 1.21.6+). Buttons whose `custom` or `dynamic/custom` action has
// an `id` without a namespace come back to
// `exports_rift_plugin_handler_dialog`. Returns whether the dialog was
// queued: false for an offline or pre-1.21.6 player, and for an invalid
// dialog, whose error is logged.
static inline bool rift_show_dialog(const rift_string_t *target, const char *dialog_json) {
  rift_string_t t = *target, json = rift_str(dialog_json), err = {NULL, 0};
  bool queued = false;
  if (!rift_plugin_players_dialog(&t, &json, &queued, &err)) {
    rift__host_error("players.dialog", &err);
    return false;
  }
  return queued;
}

// ---------------------------------------------------------------------------
// Remembered players and languages

static inline void rift_player_record_free(rift_player_record_t *record) {
  rift_plugin_types_player_record_free(record);
  memset(record, 0, sizeof *record);
}

static inline void rift_player_record_list_free(rift_player_record_list_t *list) {
  rift_plugin_players_list_player_record_free(list);
  list->ptr = NULL;
  list->len = 0;
}

// Reads what Rift remembers about a player who joined at least once, online
// or not, into `out`. `target` is a UUID or the name the player last joined
// with (case-insensitive). Returns false, with `out` zeroed, for a player
// Rift never saw. Free `out` with `rift_player_record_free`.
static inline bool rift_player_lookup(const rift_string_t *target, rift_player_record_t *out) {
  rift_string_t t = *target, err = {NULL, 0};
  rift_plugin_players_option_player_record_t found;
  memset(&found, 0, sizeof found);
  memset(out, 0, sizeof *out);
  if (!rift_plugin_players_lookup(&t, &found, &err)) {
    rift__host_error("players.lookup", &err);
    return false;
  }
  if (!found.is_some) return false;
  *out = found.val;
  return true;
}

// Reads the remembered players whose name starts with `prefix` into `out`,
// most recently seen first, at most `limit` (1..=100). Returns false, with
// `out` empty, on a host error. Free `out` with
// `rift_player_record_list_free`.
static inline bool rift_player_search(const char *prefix, uint32_t limit, rift_player_record_list_t *out) {
  rift_string_t p = rift_str(prefix), err = {NULL, 0};
  out->ptr = NULL;
  out->len = 0;
  if (!rift_plugin_players_search(&p, limit, out, &err)) {
    rift__host_error("players.search", &err);
    out->ptr = NULL;
    out->len = 0;
    return false;
  }
  return true;
}

// Sets the language Rift's own messages use for a player, online or not: a
// tag of 2..=16 bytes of `a-z0-9_` such as "de" or "de_de"; NULL goes back to
// the client's locale. `target` is a UUID or a name Rift has seen. The
// language is remembered across sessions and shows up as `ctx->language`
// from `login` on. Returns whether the player is known.
static inline bool rift_set_language(const rift_string_t *target, const char *language) {
  rift_string_t t = *target, lang = rift_str(language), err = {NULL, 0};
  bool known = false;
  if (!rift_plugin_players_set_language(&t, language ? &lang : NULL, &known, &err)) {
    rift__host_error("players.set-language", &err);
    return false;
  }
  return known;
}

// ---------------------------------------------------------------------------
// Text conversions
//
// What `rift.text` does in Lua: Rift converts `text` (borrowed) with markup,
// legacy codes and messages resolved, the plugin's own messages by
// unqualified key, in `language` ("de", "de_de"; NULL for the default). On
// success `*out` holds the result, owned by you (free it with
// `rift_string_free`), and the function returns true. On failure it returns
// false with `*out` empty; the host's error goes to `*err`, owned by you,
// or, when `err` is NULL, to the log at warn level. Use `rift_text_render` in
// `init` to check a template from the configuration:
//
//   // In exports_rift_plugin_handler_init(config_json, ret, err):
//   rift_string_t json;
//   if (!rift_text_render(rift_text_json(template_json), NULL, &json, err)) {
//     return false;  // init fails with the host's error
//   }
//   rift_string_free(&json);

static inline bool rift__text_call(bool (*call)(rift_plugin_text_text_t *, rift_string_t *, rift_string_t *,
                                                rift_string_t *),
                                   const char *function, rift_text_t text, const char *language,
                                   rift_string_t *out, rift_string_t *err) {
  rift_string_t lang = rift_str(language), failure = {NULL, 0};
  out->ptr = NULL;
  out->len = 0;
  if (err) {
    err->ptr = NULL;
    err->len = 0;
  }
  if (!call(&text, language ? &lang : NULL, out, &failure)) {
    out->ptr = NULL;
    out->len = 0;
    if (err) {
      *err = failure;
    } else {
      rift__host_error(function, &failure);
    }
    return false;
  }
  return true;
}

// The JSON text component `text` becomes; plain text becomes a JSON string.
static inline bool rift_text_render(rift_text_t text, const char *language, rift_string_t *out,
                                    rift_string_t *err) {
  return rift__text_call(rift_plugin_text_render, "text.render", text, language, out, err);
}

// The words of `text` without formatting.
static inline bool rift_text_to_plain(rift_text_t text, const char *language, rift_string_t *out,
                                      rift_string_t *err) {
  return rift__text_call(rift_plugin_text_plain, "text.plain", text, language, out, err);
}

// `text` with `§` codes, for surfaces that only take strings.
static inline bool rift_text_to_legacy(rift_text_t text, const char *language, rift_string_t *out,
                                       rift_string_t *err) {
  return rift__text_call(rift_plugin_text_to_legacy, "text.to-legacy", text, language, out, err);
}

// Markup that writes `text`; plain text comes back escaped.
static inline bool rift_text_to_mini(rift_text_t text, const char *language, rift_string_t *out,
                                     rift_string_t *err) {
  return rift__text_call(rift_plugin_text_to_mini, "text.to-mini", text, language, out, err);
}

// ---------------------------------------------------------------------------
// Permissions

// Adds the user to `group`, until `expires_at` Unix seconds (replacing an
// earlier expiry); 0 means for good. Returns whether anything changed.
static inline bool rift_add_group(const rift_string_t *uuid, const char *group, uint64_t expires_at) {
  rift_string_t u = *uuid, g = rift_str(group), err = {NULL, 0};
  bool changed = false;
  if (!rift_plugin_permissions_add_group(&u, &g, expires_at ? &expires_at : NULL, &changed, &err)) {
    rift__host_error("permissions.add-group", &err);
    return false;
  }
  return changed;
}

// ---------------------------------------------------------------------------
// Background work
//
// A plugin runs one call at a time, so a slow lookup in `login` would keep
// every other hook waiting. Instead the hook defers its answer, queues a job
// and returns; a background worker runs the job, and the answer follows from
// `job_done`:
//
// 1. In `login`, `initial_server`, `before_transfer`, `command`, `chat` or
//    `kick`, call `rift_background_defer(&ticket)`. What the hook writes into
//    `ret` is then ignored (still write `rift_decision_continue(ret)`).
// 2. `rift_background_run(payload, len, &handle)` queues the job. Remember
//    which ticket belongs to the handle.
// 3. A worker calls `exports_rift_plugin_handler_work(payload, ret, err)`. It
//    may block. A worker is another instance of the component with its own
//    memory and globals: it never sees `init` or any hook, only
//    `exports_rift_plugin_handler_work_init` once, with the configuration,
//    and then jobs. Return the result with `rift_bytes_dup`.
// 4. The main instance gets `exports_rift_plugin_handler_job_done(handle,
//    outcome)` and answers with `rift_background_resolve(ticket, &decision)`.
//
// The hook's deadline keeps running while the job does: an answer after it
// counts as a missed deadline. Workers need `[background] workers = N` in
// plugin.toml; a job may take 30 s unless the manifest sets `deadlines.work`.

// Defers the answer of the decision hook that is running and writes the
// ticket for `rift_background_resolve`. False outside a decision hook or
// when the call was deferred already.
static inline bool rift_background_defer(uint64_t *ticket) {
  rift_string_t err = {NULL, 0};
  *ticket = 0;
  if (!rift_plugin_background_defer(ticket, &err)) {
    rift__host_error("background.defer", &err);
    return false;
  }
  return true;
}

// Answers a deferred hook with `decision` and frees the decision, which is
// `continue` afterwards. Returns false when nobody waits for the ticket any
// more: the deadline passed, or it was answered.
static inline bool rift_background_resolve(uint64_t ticket, rift_decision_t *decision) {
  rift_string_t err = {NULL, 0};
  bool waiting = false;
  bool ok = rift_plugin_background_resolve(ticket, decision, &waiting, &err);
  rift_decision_free(decision);
  if (!ok) {
    rift__host_error("background.resolve", &err);
    return false;
  }
  return waiting;
}

// Queues `len` bytes of `payload` (borrowed) for a background worker and
// writes the handle `job_done` is called with. False without workers in the
// manifest or when 1024 jobs wait already.
static inline bool rift_background_run(const void *payload, size_t len, uint64_t *handle) {
  rift_bytes_t bytes = rift_bytes_view(payload, len);
  rift_string_t err = {NULL, 0};
  *handle = 0;
  if (!rift_plugin_background_run(&bytes, handle, &err)) {
    rift__host_error("background.run", &err);
    return false;
  }
  return true;
}

// Whether this instance is a background worker.
static inline bool rift_background_is_worker(void) { return rift_plugin_background_is_worker(); }

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
