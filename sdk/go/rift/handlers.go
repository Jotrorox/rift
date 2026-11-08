package rift

import (
	"go.bytecodealliance.org/cm"

	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/handler"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/types"
)

// The registered handlers. Each starts as a no-op default; the setters below
// replace them.
var (
	onInit           = func(string) (Registration, error) { return Registration{}, nil }
	onReload         = func(string) error { return nil }
	onShutdown       = func(string) {}
	onRoute          = func(Connection) RouteDecision { return RouteDefault() }
	onLogin          = func(Context) Action { return Continue() }
	onInitialServer  = func(Context) Action { return Continue() }
	onBeforeTransfer = func(Context) Action { return Continue() }
	onCommand        = func(Context) Action { return Continue() }
	onSuggest        = func(Context) []Suggestion { return nil }
	onStatus         = func(Ping) *StatusOverride { return nil }
	onHTTP           = func(HTTPRequest) *HTTPResponse { return nil }
	onJoin           = func(Context) {}
	onAfterTransfer  = func(Context) {}
	onDisconnect     = func(Context) {}
	onMessage        = func(BusMessage) {}
	onTimer          = func(uint64) {}
	onEvent          = func(Event) {}
)

// handled records the hooks that have a handler, for registrations that
// leave Hooks nil.
var handled [HookEvent + 1]bool

// OnInit sets the handler called once per instance with the plugin
// configuration as a JSON object ("{}" without one). Create subscriptions and
// timers here. A returned error fails the plugin load.
//
// When the returned Registration.Hooks is nil, the SDK fills in the hooks
// that have a handler set through the On* functions, so the host skips the
// rest. A non-nil list, even an empty one (every hook), is sent unchanged.
func OnInit(f func(configJSON string) (Registration, error)) { onInit = f }

// OnReload sets the handler called when the configuration changes and the
// manifest sets hot_reload = true.
func OnReload(f func(configJSON string) error) { onReload = f }

// OnShutdown sets the handler called before the instance is dropped.
func OnShutdown(f func(reason string)) { onShutdown = f }

// OnRoute sets the handler that picks a backend right after TCP accept.
func OnRoute(f func(Connection) RouteDecision) {
	onRoute = f
	handled[HookRoute] = true
}

// OnLogin sets the handler that may deny an authenticated login.
func OnLogin(f func(Context) Action) {
	onLogin = f
	handled[HookLogin] = true
}

// OnInitialServer sets the handler that may pick the first server.
func OnInitialServer(f func(Context) Action) {
	onInitialServer = f
	handled[HookInitialServer] = true
}

// OnBeforeTransfer sets the handler that may deny a server switch.
func OnBeforeTransfer(f func(Context) Action) {
	onBeforeTransfer = f
	handled[HookBeforeTransfer] = true
}

// OnCommand sets the handler for registered commands.
func OnCommand(f func(Context) Action) {
	onCommand = f
	handled[HookCommand] = true
}

// OnSuggest sets the handler for commands registered with SuggestDynamic.
func OnSuggest(f func(Context) []Suggestion) {
	onSuggest = f
	handled[HookSuggest] = true
}

// OnStatus sets the handler that may override a server-list ping response.
func OnStatus(f func(Ping) *StatusOverride) {
	onStatus = f
	handled[HookStatus] = true
}

// OnHTTP sets the handler for /api/script/* requests; nil means not handled.
func OnHTTP(f func(HTTPRequest) *HTTPResponse) {
	onHTTP = f
	handled[HookHTTP] = true
}

// OnJoin sets the handler called after a player joined.
func OnJoin(f func(Context)) {
	onJoin = f
	handled[HookJoin] = true
}

// OnAfterTransfer sets the handler called after a server switch.
func OnAfterTransfer(f func(Context)) {
	onAfterTransfer = f
	handled[HookAfterTransfer] = true
}

// OnDisconnect sets the handler called after a session closed.
func OnDisconnect(f func(Context)) {
	onDisconnect = f
	handled[HookDisconnect] = true
}

// OnMessage sets the handler for messages on subscriptions made with
// Subscribe.
func OnMessage(f func(BusMessage)) {
	onMessage = f
	handled[HookMessage] = true
}

// OnTimer sets the handler for timers started with Every or After.
func OnTimer(f func(handle uint64)) {
	onTimer = f
	handled[HookTimer] = true
}

// OnEvent sets the handler for rift.events.* publications.
func OnEvent(f func(Event)) {
	onEvent = f
	handled[HookEvent] = true
}

func init() {
	e := &handler.Exports
	e.Init = func(configJSON string) cm.Result[handler.RegistrationShape, types.Registration, string] {
		reg, err := onInit(configJSON)
		if err != nil {
			return cm.Err[cm.Result[handler.RegistrationShape, types.Registration, string]](err.Error())
		}
		if reg.Hooks == nil {
			for hook, ok := range handled {
				if ok {
					reg.Hooks = append(reg.Hooks, Hook(hook))
				}
			}
		}
		return cm.OK[cm.Result[handler.RegistrationShape, types.Registration, string]](reg.lower())
	}
	e.Reload = func(configJSON string) cm.Result[string, struct{}, string] {
		if err := onReload(configJSON); err != nil {
			return cm.Err[cm.Result[string, struct{}, string]](err.Error())
		}
		return cm.OK[cm.Result[string, struct{}, string]](struct{}{})
	}
	e.Shutdown = func(reason string) { onShutdown(reason) }
	e.Route = func(c types.Connection) types.RouteDecision { return onRoute(liftConnection(c)).lower() }
	e.Login = func(c types.Context) types.Action { return onLogin(liftContext(c)).lower() }
	e.InitialServer = func(c types.Context) types.Action { return onInitialServer(liftContext(c)).lower() }
	e.BeforeTransfer = func(c types.Context) types.Action { return onBeforeTransfer(liftContext(c)).lower() }
	e.Command = func(c types.Context) types.Action { return onCommand(liftContext(c)).lower() }
	e.Suggest = func(c types.Context) cm.List[types.Suggestion] {
		suggestions := onSuggest(liftContext(c))
		out := make([]types.Suggestion, len(suggestions))
		for i, s := range suggestions {
			out[i] = types.Suggestion{Text: s.Text, Tooltip: lowerOptString(s.Tooltip)}
		}
		return cm.ToList(out)
	}
	e.Status = func(p types.Ping) cm.Option[types.StatusOverride] {
		if o := onStatus(liftPing(p)); o != nil {
			return cm.Some(o.lower())
		}
		return cm.None[types.StatusOverride]()
	}
	e.HTTP = func(r types.HTTPRequest) cm.Option[types.HTTPResponse] {
		if resp := onHTTP(liftHTTPRequest(r)); resp != nil {
			return cm.Some(types.HTTPResponse{Status: resp.Status, ContentType: resp.ContentType, Body: resp.Body})
		}
		return cm.None[types.HTTPResponse]()
	}
	e.Join = func(c types.Context) { onJoin(liftContext(c)) }
	e.AfterTransfer = func(c types.Context) { onAfterTransfer(liftContext(c)) }
	e.Disconnect = func(c types.Context) { onDisconnect(liftContext(c)) }
	e.Message = func(m types.BusMessage) { onMessage(liftBusMessage(m)) }
	e.Timer = func(h uint64) { onTimer(h) }
	e.Event = func(ev types.RiftEvent) { onEvent(Event{Subject: ev.Subject, PayloadJSON: ev.PayloadJSON}) }
}
