// Weak no-op implementations of every `rift:plugin/handler` export.
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
