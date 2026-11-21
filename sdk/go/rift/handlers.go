package rift

import (
	"errors"

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
	onLogin          = func(Context) Decision { return Continue() }
	onInitialServer  = func(Context) Decision { return Continue() }
	onBeforeTransfer = func(Context) Decision { return Continue() }
	onCommand        = func(Context) Decision { return Continue() }
	onSuggest        = func(Context) []Suggestion { return nil }
	onStatus         = func(Ping) *StatusOverride { return nil }
	onHTTP           = func(HTTPRequest) *HTTPResponse { return nil }
	onChat           = func(ChatAttempt) Decision { return Continue() }
	onKick           = func(Context) Decision { return Continue() }
	onJoin           = func(Context) {}
	onAfterTransfer  = func(Context) {}
	onDisconnect     = func(Context) {}
	onMessage        = func(BusMessage) {}
	onTimer          = func(uint64) {}
	onEvent          = func(Event) {}
	onDialog         = func(DialogResponse) {}
	onMenu           = func(MenuEvent) {}
	onJobDone        = func(uint64, []byte, error) {}
	onWorkInit       = func(string) error { return nil }
	onWork           = func([]byte) ([]byte, error) { return nil, ErrNoWork }
)

// ErrNoWork is what the default work handler answers every job with.
var ErrNoWork = errors.New("this plugin has no background work")

// JobError is why a job started with Run failed: the error its OnWork
// handler returned, or the host's reason (a trap, a missed deadline).
type JobError string

func (e JobError) Error() string { return string(e) }

// handled records the hooks that have a handler, for registrations that
// leave Hooks nil.
var handled [HookKick + 1]bool

// OnInit sets the handler called once per instance with the plugin
// configuration as a JSON object ("{}" without one). Create subscriptions and
// timers here. A returned error fails the plugin load.
//
// When the returned Registration.Hooks is nil, the SDK fills in the hooks
// that have a handler set through the On* functions, so the host skips the
// rest. A non-nil list is sent unchanged; an empty one delivers every hook
// except chat and kick, which must be listed.
//
// A background worker instance never runs this handler; see OnWorkInit.
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
func OnLogin(f func(Context) Decision) {
	onLogin = f
	handled[HookLogin] = true
}

// OnInitialServer sets the handler that may pick the first server.
func OnInitialServer(f func(Context) Decision) {
	onInitialServer = f
	handled[HookInitialServer] = true
}

// OnBeforeTransfer sets the handler that may deny a server switch.
func OnBeforeTransfer(f func(Context) Decision) {
	onBeforeTransfer = f
	handled[HookBeforeTransfer] = true
}

// OnCommand sets the handler for registered commands.
func OnCommand(f func(Context) Decision) {
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

// OnHTTP sets the handler for /ext and /ext/* requests; nil means not handled.
func OnHTTP(f func(HTTPRequest) *HTTPResponse) {
	onHTTP = f
	handled[HookHTTP] = true
}

// OnChat sets the handler that sees every chat message, and every command
// that reaches the backend, before the backend does. Continue passes the
// packet on unchanged; Deny drops it and shows the player the text. For a
// command, attempt.Ctx.Command is the root name and attempt.Ctx.Args the
// rest; commands that Rift or a plugin owns never come here.
//
// Rift asks only a plugin whose registration lists HookChat (the SDK lists it
// when Hooks is nil). Dropping one signed message breaks the player's
// signature chain, so unless the proxy sets authentication.signed_chat =
// false, the first deny costs that player their chat signing until they
// reconnect. The deadline is 50 ms unless the manifest sets deadlines.chat.
func OnChat(f func(ChatAttempt) Decision) {
	onChat = f
	handled[HookChat] = true
}

// OnKick sets the handler called when a backend kicks a player in play.
// ctx.Server is the backend and ctx.Reason its reason without formatting.
// Continue leaves it to network.on_kick, Deny disconnects the player with the
// text, Server moves them there. The registration must list HookKick (the SDK
// lists it when Hooks is nil).
func OnKick(f func(Context) Decision) {
	onKick = f
	handled[HookKick] = true
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

// OnDialog sets the handler for clicks on plugin actions of dialogs shown
// with ShowDialog. It needs no hook in the registration.
func OnDialog(f func(DialogResponse)) { onDialog = f }

// OnMenu sets the handler for everything a player does in a menu shown with
// ShowMenu: MenuEvent.Kind says whether it was a click, typing or the end of
// the menu. Ignore the kinds you do not know. It needs no hook in the
// registration.
func OnMenu(f func(MenuEvent)) { onMenu = f }

// OnJobDone sets the handler called when a job started with Run finished:
// result is what the worker's OnWork handler returned, or err is a JobError
// saying why it failed. It needs no hook in the registration.
func OnJobDone(f func(handle uint64, result []byte, err error)) { onJobDone = f }

// OnWorkInit sets the handler a background worker runs once before its first
// job, with the plugin's configuration. Each worker (manifest [background]
// workers) is its own instance of the component with its own memory: package
// init functions run, but it receives no hooks, never runs the OnInit
// handler and makes no registration.
func OnWorkInit(f func(configJSON string) error) { onWorkInit = f }

// OnWork sets the handler that runs one job queued with Run, on a background
// worker. It may block: the plugin's hooks keep running meanwhile. The limit
// is 30 s unless the manifest sets deadlines.work. Without a handler every
// job fails with ErrNoWork.
func OnWork(f func(payload []byte) ([]byte, error)) { onWork = f }

type (
	unitResult  = cm.Result[string, struct{}, string]
	bytesResult = cm.Result[cm.List[uint8], cm.List[uint8], string]
)

func lowerUnit(err error) unitResult {
	if err != nil {
		return cm.Err[unitResult](err.Error())
	}
	return cm.OK[unitResult](struct{}{})
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
	e.Reload = func(configJSON string) unitResult { return lowerUnit(onReload(configJSON)) }
	e.Shutdown = func(reason string) { onShutdown(reason) }
	e.Route = func(c types.Connection) types.RouteDecision { return onRoute(liftConnection(c)).lower() }
	e.Login = func(c types.Context) types.Decision { return onLogin(liftContext(c)).lower() }
	e.InitialServer = func(c types.Context) types.Decision { return onInitialServer(liftContext(c)).lower() }
	e.BeforeTransfer = func(c types.Context) types.Decision { return onBeforeTransfer(liftContext(c)).lower() }
	e.Command = func(c types.Context) types.Decision { return onCommand(liftContext(c)).lower() }
	e.Suggest = func(c types.Context) cm.List[types.Suggestion] {
		suggestions := onSuggest(liftContext(c))
		out := make([]types.Suggestion, len(suggestions))
		for i, s := range suggestions {
			out[i] = types.Suggestion{Text: s.Text, Tooltip: s.Tooltip.lowerOpt()}
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
	e.Chat = func(a types.ChatAttempt) types.Decision { return onChat(liftChatAttempt(a)).lower() }
	e.Kick = func(c types.Context) types.Decision { return onKick(liftContext(c)).lower() }
	e.Join = func(c types.Context) { onJoin(liftContext(c)) }
	e.AfterTransfer = func(c types.Context) { onAfterTransfer(liftContext(c)) }
	e.Disconnect = func(c types.Context) { onDisconnect(liftContext(c)) }
	e.Message = func(m types.BusMessage) { onMessage(liftBusMessage(m)) }
	e.Timer = func(h uint64) { onTimer(h) }
	e.Event = func(ev types.RiftEvent) { onEvent(Event{Subject: ev.Subject, PayloadJSON: ev.PayloadJSON}) }
	e.Dialog = func(r types.DialogResponse) { onDialog(liftDialogResponse(r)) }
	e.Menu = func(ev types.MenuEvent) { onMenu(liftMenuEvent(ev)) }
	e.JobDone = func(handle uint64, outcome bytesResult) {
		if err := outcome.Err(); err != nil {
			onJobDone(handle, nil, JobError(*err))
			return
		}
		onJobDone(handle, outcome.OK().Slice(), nil)
	}
	e.WorkInit = func(configJSON string) unitResult { return lowerUnit(onWorkInit(configJSON)) }
	e.Work = func(payload cm.List[uint8]) bytesResult {
		result, err := onWork(payload.Slice())
		if err != nil {
			return cm.Err[bytesResult](err.Error())
		}
		return cm.OK[bytesResult](cm.ToList(result))
	}
}
