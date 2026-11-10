// Weak no-op implementations of every `rift:plugin/handler` export and of
// `rift:plugin/dialogs` (dialog clicks, since 0.1.1), weak
// `rift:plugin/decisions` exports (since 0.1.2) that answer with the
// handler's decision hooks, and weak `rift:plugin/styled` exports (since
// 0.1.3) that answer with the handler's `suggest` and `status`.
//
// Link this file next to the generated `rift_plugin.c` and define only the
// hooks your plugin needs; a strong definition in your own source replaces
// the default here. Each default frees its (owned) arguments and returns the
// neutral answer: an empty registration, `continue`, the configured route,
// no suggestions, no status override and no HTTP response.
//
// Remember to list the hooks you implement in the registration `init`
// returns: an empty hook list delivers every hook, a non-empty one only the
// listed hooks.
#include "rift.h"

#define RIFT_WEAK __attribute__((weak))

RIFT_WEAK bool exports_rift_plugin_handler_init(rift_plugin_string_t *config_json,
                                                exports_rift_plugin_handler_registration_t *ret,
                                                rift_plugin_string_t *err) {
  (void)err;
  rift_string_free(config_json);
  rift_registration_init(ret);
  return true;
}

RIFT_WEAK bool exports_rift_plugin_handler_reload(rift_plugin_string_t *config_json, rift_plugin_string_t *err) {
  (void)err;
  rift_string_free(config_json);
  return true;
}

RIFT_WEAK void exports_rift_plugin_handler_shutdown(rift_plugin_string_t *reason) { rift_string_free(reason); }

RIFT_WEAK void exports_rift_plugin_handler_route(exports_rift_plugin_handler_connection_t *connection,
                                                 exports_rift_plugin_handler_route_decision_t *ret) {
  rift_plugin_types_connection_free(connection);
  memset(ret, 0, sizeof *ret);
  ret->tag = RIFT_PLUGIN_TYPES_ROUTE_DECISION_DEFAULT;
}

RIFT_WEAK void exports_rift_plugin_handler_login(exports_rift_plugin_handler_context_t *ctx,
                                                 exports_rift_plugin_handler_action_t *ret) {
  rift_context_free(ctx);
  rift_action_continue(ret);
}

RIFT_WEAK void exports_rift_plugin_handler_initial_server(exports_rift_plugin_handler_context_t *ctx,
                                                          exports_rift_plugin_handler_action_t *ret) {
  rift_context_free(ctx);
  rift_action_continue(ret);
}

RIFT_WEAK void exports_rift_plugin_handler_before_transfer(exports_rift_plugin_handler_context_t *ctx,
                                                           exports_rift_plugin_handler_action_t *ret) {
  rift_context_free(ctx);
  rift_action_continue(ret);
}

RIFT_WEAK void exports_rift_plugin_handler_command(exports_rift_plugin_handler_context_t *ctx,
                                                   exports_rift_plugin_handler_action_t *ret) {
  rift_context_free(ctx);
  rift_action_continue(ret);
}

RIFT_WEAK void exports_rift_plugin_handler_suggest(exports_rift_plugin_handler_context_t *ctx,
                                                   exports_rift_plugin_handler_list_suggestion_t *ret) {
  rift_context_free(ctx);
  ret->ptr = NULL;
  ret->len = 0;
}

RIFT_WEAK bool exports_rift_plugin_handler_status(exports_rift_plugin_handler_ping_t *ping,
                                                  exports_rift_plugin_handler_status_override_t *ret) {
  (void)ret;
  rift_plugin_types_ping_free(ping);
  return false;
}

RIFT_WEAK bool exports_rift_plugin_handler_http(exports_rift_plugin_handler_http_request_t *request,
                                                exports_rift_plugin_handler_http_response_t *ret) {
  (void)ret;
  rift_plugin_types_http_request_free(request);
  return false;
}

RIFT_WEAK void exports_rift_plugin_handler_join(exports_rift_plugin_handler_context_t *ctx) { rift_context_free(ctx); }

RIFT_WEAK void exports_rift_plugin_handler_after_transfer(exports_rift_plugin_handler_context_t *ctx) {
  rift_context_free(ctx);
}

RIFT_WEAK void exports_rift_plugin_handler_disconnect(exports_rift_plugin_handler_context_t *ctx) {
  rift_context_free(ctx);
}

RIFT_WEAK void exports_rift_plugin_handler_message(exports_rift_plugin_handler_bus_message_t *message) {
  rift_bus_message_free(message);
}

RIFT_WEAK void exports_rift_plugin_handler_timer(uint64_t handle) { (void)handle; }

RIFT_WEAK void exports_rift_plugin_handler_event(exports_rift_plugin_handler_rift_event_t *event) {
  rift_plugin_types_rift_event_free(event);
}

// Dialog clicks need no registration; a plugin that shows no dialogs never
// receives one.
RIFT_WEAK void exports_rift_plugin_dialogs_dialog(exports_rift_plugin_dialogs_dialog_response_t *response) {
  rift_dialog_response_free(response);
}

// Rift 0.1.2 and newer ask `decisions` instead of the handler's decision
// hooks. These defaults forward to the handler hooks, so a plugin defines
// either one; define a `decisions` hook to answer with rich text.
#define RIFT_FORWARD_DECISION(hook)                                                            \
  RIFT_WEAK void exports_rift_plugin_decisions_##hook(exports_rift_plugin_decisions_context_t *ctx, \
                                                       exports_rift_plugin_decisions_decision_t *ret) { \
    rift_action_t action;                                                                      \
    exports_rift_plugin_handler_##hook(ctx, &action);                                          \
    rift_decision_from_action(ret, &action);                                                   \
  }

RIFT_FORWARD_DECISION(login)
RIFT_FORWARD_DECISION(initial_server)
RIFT_FORWARD_DECISION(before_transfer)
RIFT_FORWARD_DECISION(command)

// Rift 0.1.3 and newer ask `styled` instead of the handler's `suggest` and
// `status`. These defaults forward to the handler hooks and show their plain
// text as written; define a `styled` hook to answer with rich text.
RIFT_WEAK void exports_rift_plugin_styled_suggest(exports_rift_plugin_styled_context_t *ctx,
                                                  exports_rift_plugin_styled_list_rich_suggestion_t *ret) {
  exports_rift_plugin_handler_list_suggestion_t plain = {NULL, 0};
  exports_rift_plugin_handler_suggest(ctx, &plain);
  ret->ptr = NULL;
  ret->len = plain.len;
  if (plain.len) ret->ptr = (rift_rich_suggestion_t *)rift_alloc(plain.len * sizeof(rift_rich_suggestion_t));
  for (size_t i = 0; i < plain.len; i++) {
    rift_plugin_types_suggestion_t *s = &plain.ptr[i];
    ret->ptr[i].text = s->text;
    ret->ptr[i].tooltip_json = s->tooltip.is_some ? rift_some(rift_string_quote(&s->tooltip.val)) : rift_none();
    rift_option_string_free(&s->tooltip);
  }
  free(plain.ptr);
}

RIFT_WEAK bool exports_rift_plugin_styled_status(exports_rift_plugin_styled_ping_t *ping,
                                                 exports_rift_plugin_styled_rich_status_override_t *ret) {
  exports_rift_plugin_handler_status_override_t plain;
  memset(&plain, 0, sizeof plain);
  if (!exports_rift_plugin_handler_status(ping, &plain)) return false;
  memset(ret, 0, sizeof *ret);
  ret->motd_json = plain.motd_json;
  ret->version_json = plain.version.is_some ? rift_some(rift_string_quote(&plain.version.val)) : rift_none();
  rift_option_string_free(&plain.version);
  ret->protocol = plain.protocol;
  ret->online = plain.online;
  ret->max_players = plain.max_players;
  if (plain.sample.is_some) {
    ret->sample_json.is_some = true;
    ret->sample_json.val = plain.sample.val;
    for (size_t i = 0; i < plain.sample.val.len; i++) {
      rift_string_t quoted = rift_string_quote(&plain.sample.val.ptr[i]);
      rift_string_free(&plain.sample.val.ptr[i]);
      ret->sample_json.val.ptr[i] = quoted;
    }
  }
  ret->favicon = plain.favicon;
  return true;
}
