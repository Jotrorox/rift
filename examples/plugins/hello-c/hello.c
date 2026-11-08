// hello-c: the C example plugin and a host conformance fixture.
//
// It behaves exactly like examples/plugins/hello-rs and hello-go, so the host
// runs one conformance suite against all three:
//
// - init(config) decodes the JSON configuration ({"deny": "<name>"}, `deny`
//   optional; invalid JSON or a non-string `deny` fails init), starts one
//   repeating 1000 ms timer with timers.every(1000), subscribes to the bus
//   subject `hello.ping` without a queue group (a failed subscription fails
//   init) and returns this registration:
//   - command `hello`, permission `hello.use`, suggest `players`;
//   - hooks login, command, join, message, timer, so the host skips every
//     other hook.
// - login(ctx) returns deny("You are not welcome here.") when ctx.name equals
//   the configured `deny` name exactly, otherwise continue.
// - command(ctx) returns message("Hello, <ctx.name>!").
// - join(ctx) calls players.message(ctx.uuid, "Welcome, <ctx.name>!"), then
//   store.increment("hello", "joins", 1). Host errors are ignored.
// - message(msg): when msg.reply is set, publishes msg.payload unchanged to
//   that subject (no reply subject of its own); otherwise does nothing.
//   Publish errors are ignored.
// - timer(handle) logs `tick` at debug level.
//
// No other hook is implemented and nothing else is logged.
//
// Build: `make` (see Makefile and sdk/c/README.md).
#include "rift.h"

static rift_string_t deny_name;
static bool deny_set;

bool exports_rift_plugin_handler_init(rift_string_t *config_json, rift_registration_t *ret, rift_string_t *err) {
  rift_string_t deny;
  rift_json_result_t parsed = rift_json_get_string(config_json, "deny", &deny);
  rift_string_free(config_json);
  switch (parsed) {
    case RIFT_JSON_OK:
      rift_string_free(&deny_name);
      deny_name = deny;
      deny_set = true;
      break;
    case RIFT_JSON_MISSING:
      rift_string_free(&deny_name);
      deny_set = false;
      break;
    case RIFT_JSON_WRONG_TYPE:
      *err = rift_string_dup("invalid configuration: `deny` must be a string");
      return false;
    case RIFT_JSON_INVALID:
      *err = rift_string_dup("invalid configuration: expected a JSON object");
      return false;
  }

  rift_plugin_timers_every(1000);

  rift_string_t subject = rift_str("hello.ping");
  uint64_t subscription;
  // Errors from host imports are owned by the caller, so hand this one back.
  if (!rift_plugin_bus_subscribe(&subject, NULL, &subscription, err)) return false;

  rift_registration_init(ret);
  rift_registration_add_command(ret, "hello", "hello.use", RIFT_PLUGIN_TYPES_SUGGEST_KIND_PLAYERS);
  rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_LOGIN);
  rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_COMMAND);
  rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_JOIN);
  rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_MESSAGE);
  rift_registration_add_hook(ret, RIFT_PLUGIN_TYPES_HOOK_TIMER);
  return true;
}

void exports_rift_plugin_handler_login(rift_context_t *ctx, rift_action_t *ret) {
  if (deny_set && rift_string_eq_string(&ctx->name, &deny_name)) {
    rift_action_deny(ret, "You are not welcome here.");
  } else {
    rift_action_continue(ret);
  }
  rift_context_free(ctx);
}

void exports_rift_plugin_handler_command(rift_context_t *ctx, rift_action_t *ret) {
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_ACTION_MESSAGE;
  ret->val.message = rift_string_fmt("Hello, " RIFT_STR_FMT "!", RIFT_STR_ARG(ctx->name));
  rift_context_free(ctx);
}

void exports_rift_plugin_handler_join(rift_context_t *ctx) {
  rift_string_t text = rift_string_fmt("Welcome, " RIFT_STR_FMT "!", RIFT_STR_ARG(ctx->name));
  rift_string_t err = {NULL, 0};
  bool online;
  if (!rift_plugin_players_message(&ctx->uuid, &text, &online, &err)) rift_string_free(&err);
  rift_string_free(&text);

  rift_string_t ns = rift_str("hello"), key = rift_str("joins");
  int64_t joins;
  if (!rift_plugin_store_increment(&ns, &key, 1, &joins, &err)) rift_string_free(&err);
  rift_context_free(ctx);
}

void exports_rift_plugin_handler_message(rift_bus_message_t *msg) {
  if (msg->reply.is_some) {
    rift_string_t err = {NULL, 0};
    uint32_t delivered;
    if (!rift_plugin_bus_publish(&msg->reply.val, &msg->payload, NULL, &delivered, &err)) rift_string_free(&err);
  }
  rift_bus_message_free(msg);
}

void exports_rift_plugin_handler_timer(uint64_t handle) {
  (void)handle;
  rift_log(RIFT_DEBUG, "tick");
}
