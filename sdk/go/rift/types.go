package rift

import (
	"strings"

	"go.bytecodealliance.org/cm"

	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/types"
)

// Hook names an event a plugin receives. List the hooks a plugin implements
// in Registration.Hooks.
type Hook = types.Hook

const (
	HookRoute          = types.HookRoute
	HookLogin          = types.HookLogin
	HookInitialServer  = types.HookInitialServer
	HookBeforeTransfer = types.HookBeforeTransfer
	HookCommand        = types.HookCommand
	HookSuggest        = types.HookSuggest
	HookStatus         = types.HookStatus
	HookHTTP           = types.HookHTTP
	HookJoin           = types.HookJoin
	HookAfterTransfer  = types.HookAfterTransfer
	HookDisconnect     = types.HookDisconnect
	HookMessage        = types.HookMessage
	HookTimer          = types.HookTimer
	HookEvent          = types.HookEvent
)

// Level is a log severity.
type Level = types.Level

const (
	LevelTrace = types.LevelTrace
	LevelDebug = types.LevelDebug
	LevelInfo  = types.LevelInfo
	LevelWarn  = types.LevelWarn
	LevelError = types.LevelError
)

// Context is the session an authenticated player hook is about. Fields that
// do not apply to a hook are nil or empty.
type Context struct {
	ConnectionID  uint64
	UUID          string // 32 lowercase hexadecimal digits, without dashes
	Name          string
	Listener      string
	Hostname      string
	PeerIP        string
	Protocol      int32
	Server        *string
	DefaultServer *string
	Target        *string // before-transfer, after-transfer: the destination
	Reason        *string // before-transfer: why; disconnect: why the session closed
	Success       *bool   // after-transfer: whether the transfer completed
	Command       *string // command, suggest: the command name
	Args          string  // command, suggest: the text after the command name
	Word          string  // suggest: the argument being completed
	Position      uint32  // suggest: its 1-based position
}

// Connection is the metadata available right after TCP accept.
type Connection struct {
	Listener       string
	PeerAddr       string
	PeerIP         string
	PeerPort       uint16
	LocalAddr      string
	LocalIP        string
	LocalPort      uint16
	DefaultBackend *string
}

// RouteDecision answers the route hook. Build one with RouteDefault,
// RouteBackend or RouteReject.
type RouteDecision struct {
	kind   uint8
	value  string
	reason *string
}

// RouteDefault keeps the configured routing policy.
func RouteDefault() RouteDecision { return RouteDecision{} }

// RouteBackend sends the connection to a configured backend or group.
func RouteBackend(name string) RouteDecision { return RouteDecision{kind: 1, value: name} }

// RouteReject closes the connection, with an optional reason.
func RouteReject(reason *string) RouteDecision { return RouteDecision{kind: 2, reason: reason} }

func (d RouteDecision) lower() types.RouteDecision {
	switch d.kind {
	case 1:
		return types.RouteDecisionBackend(d.value)
	case 2:
		return types.RouteDecisionReject(lowerOptString(d.reason))
	default:
		return types.RouteDecisionDefault()
	}
}

// Action answers a decision hook. Each hook accepts a subset:
//   - login, before-transfer: Continue, Deny, DenyJSON
//   - initial-server: Continue, Deny, DenyJSON, Server
//   - command: Continue, Message, MessageJSON, Server, Queue, LeaveQueue
type Action struct {
	kind  uint8
	value string
}

const (
	actionContinue uint8 = iota
	actionDeny
	actionServer
	actionMessage
	actionQueue
	actionLeaveQueue
	actionDenyJSON
	actionMessageJSON
)

// Continue lets the event proceed unchanged.
func Continue() Action { return Action{kind: actionContinue} }

// Deny refuses the login or transfer with a reason shown to the player.
func Deny(reason string) Action { return Action{kind: actionDeny, value: reason} }

// DenyJSON refuses the login or transfer with a JSON text component of at
// most 32 KiB, for colours, line breaks and hover text.
func DenyJSON(reason string) Action { return Action{kind: actionDenyJSON, value: reason} }

// Server sends the player to a server.
func Server(name string) Action { return Action{kind: actionServer, value: name} }

// Message replies to a command with a chat message.
func Message(text string) Action { return Action{kind: actionMessage, value: text} }

// MessageJSON replies to a command with a JSON text component of at most
// 32 KiB, for colours, hover text and click events.
func MessageJSON(text string) Action { return Action{kind: actionMessageJSON, value: text} }

// Queue puts the player into a server's queue.
func Queue(server string) Action { return Action{kind: actionQueue, value: server} }

// LeaveQueue removes the player from their queue.
func LeaveQueue() Action { return Action{kind: actionLeaveQueue} }

func (a Action) lower() types.Action {
	switch a.kind {
	case actionDeny:
		return types.ActionDeny(a.value)
	case actionServer:
		return types.ActionServer(a.value)
	case actionMessage:
		return types.ActionMessage(a.value)
	case actionQueue:
		return types.ActionQueue(a.value)
	case actionLeaveQueue:
		return types.ActionLeaveQueue()
	// Hosts before rift:plugin 0.1.2 call the handler hooks and have no
	// rich text: they get the component's words.
	case actionDenyJSON:
		return types.ActionDeny(componentWords(a.value))
	case actionMessageJSON:
		return types.ActionMessage(componentWords(a.value))
	default:
		return types.ActionContinue()
	}
}

// lowerDecision is the answer of the decisions export (rift:plugin 0.1.2).
func (a Action) lowerDecision() types.Decision {
	switch a.kind {
	case actionDeny:
		return types.DecisionDeny(a.value)
	case actionServer:
		return types.DecisionServer(a.value)
	case actionMessage:
		return types.DecisionMessage(a.value)
	case actionQueue:
		return types.DecisionQueue(a.value)
	case actionLeaveQueue:
		return types.DecisionLeaveQueue()
	case actionDenyJSON:
		return types.DecisionDenyJSON(a.value)
	case actionMessageJSON:
		return types.DecisionMessageJSON(a.value)
	default:
		return types.DecisionContinue()
	}
}

// componentWords is the visible text of a JSON text component, without
// formatting; text that is not a valid component is returned as it is. It
// uses the configuration scanner rather than encoding/json, which would add
// hundreds of kilobytes to every plugin.
func componentWords(text string) string {
	p := &jsonScanner{s: text}
	var out strings.Builder
	if !p.words(&out, 0) {
		return text
	}
	p.ws()
	if p.i != len(p.s) {
		return text
	}
	return out.String()
}

// Suggestion is one completion offered by the suggest hook.
type Suggestion struct {
	Text    string
	Tooltip *string
}

// SuggestKind says how a command's arguments are completed.
type SuggestKind struct {
	kind  uint8
	fixed []string
}

// SuggestNone offers no completions.
func SuggestNone() SuggestKind { return SuggestKind{} }

// SuggestFixed offers a fixed list, filtered by prefix.
func SuggestFixed(choices ...string) SuggestKind { return SuggestKind{kind: 1, fixed: choices} }

// SuggestPlayers offers online player names.
func SuggestPlayers() SuggestKind { return SuggestKind{kind: 2} }

// SuggestServers offers servers the player may join.
func SuggestServers() SuggestKind { return SuggestKind{kind: 3} }

// SuggestDynamic asks the plugin's suggest hook.
func SuggestDynamic() SuggestKind { return SuggestKind{kind: 4} }

func (s SuggestKind) lower() types.SuggestKind {
	switch s.kind {
	case 1:
		return types.SuggestKindFixed(cm.ToList(s.fixed))
	case 2:
		return types.SuggestKindPlayers()
	case 3:
		return types.SuggestKindServers()
	case 4:
		return types.SuggestKindDynamic()
	default:
		return types.SuggestKindNone()
	}
}

// CommandSpec registers a proxy command.
type CommandSpec struct {
	Name       string // 1..=64 bytes of a-z0-9_-; "server" and "hub" are reserved
	Permission string // the node a player needs to see and run the command
	Suggest    SuggestKind
}

// Registration is what the init hook returns.
type Registration struct {
	Commands []CommandSpec
	// Hooks to deliver. An empty list delivers every hook.
	Hooks []Hook
}

func (r Registration) lower() types.Registration {
	commands := make([]types.CommandSpec, len(r.Commands))
	for i, c := range r.Commands {
		commands[i] = types.CommandSpec{Name: c.Name, Permission: c.Permission, Suggest: c.Suggest.lower()}
	}
	return types.Registration{Commands: cm.ToList(commands), Hooks: cm.ToList(r.Hooks)}
}

// Ping is a server-list ping, before any backend contact.
type Ping struct {
	Listener    string
	Hostname    string
	Protocol    int32
	PeerIP      string
	Backend     *string // the routed backend or group; nil for an unmatched hostname
	Maintenance bool
	Online      uint64
	MaxPlayers  uint64
}

// StatusOverride replaces fields of one ping response; nil fields keep the
// configured value.
type StatusOverride struct {
	MotdJSON   *string // a JSON text component or a JSON string
	Version    *string
	Protocol   *int32
	Online     *uint32
	MaxPlayers *uint32
	Sample     []string // nil keeps the sample; use an empty slice to clear it
	Favicon    *string  // a data:image/png;base64,... URI
}

func (s StatusOverride) lower() types.StatusOverride {
	out := types.StatusOverride{
		MotdJSON:   lowerOptString(s.MotdJSON),
		Version:    lowerOptString(s.Version),
		Protocol:   lowerOpt(s.Protocol),
		Online:     lowerOpt(s.Online),
		MaxPlayers: lowerOpt(s.MaxPlayers),
		Favicon:    lowerOptString(s.Favicon),
	}
	if s.Sample != nil {
		out.Sample = cm.Some(cm.ToList(s.Sample))
	}
	return out
}

// HTTPRequest is an authenticated request to /api/script/* on the web server.
type HTTPRequest struct {
	Method      string
	Path        string
	Query       string
	Body        string
	Headers     [][2]string
	ContextJSON string // the authenticated principal and scopes, as a JSON object
}

// HTTPResponse answers an HTTPRequest.
type HTTPResponse struct {
	Status      uint16
	ContentType string
	Body        string
}

// BusMessage is a message delivered to a subscription made with Subscribe.
type BusMessage struct {
	Handle  uint64 // the handle Subscribe returned
	Subject string
	Payload []byte
	Reply   *string
}

// DialogResponse is a click on a plugin action of a dialog this plugin
// showed with ShowDialog.
type DialogResponse struct {
	Ctx    Context // the player; Server is their current backend
	Action string  // the action's id as the plugin wrote it, such as "submit"
	// ValuesJSON holds the player's input values by key as a JSON object
	// ("{}" for a custom action). They come from the client: validate them
	// like any player input.
	ValuesJSON string
	// PayloadJSON is the action's payload or additions as the plugin wrote
	// it, or nil. Rift keeps it, so the client can neither see nor change it.
	PayloadJSON *string
}

// Event mirrors a rift.events.* publication.
type Event struct {
	Subject     string // for example rift.events.players
	PayloadJSON string // the event body as a JSON object
}

// PlayerInfo is a joined player in the process-wide directory.
type PlayerInfo struct {
	ConnectionID uint64
	UUID         string
	Name         string
	Server       string // the backend the player is connected to
	Protocol     int32
}

// Title is a title shown to one player; times are ticks.
type Title struct {
	Title    string
	Subtitle *string
	FadeIn   int32
	Stay     int32
	FadeOut  int32
}

// BossBar is a boss bar shown to one player.
type BossBar struct {
	TextJSON  string  // a plain string or a JSON text component
	Progress  float32 // 0.0 to 1.0
	Color     string  // pink, blue, red, green, yellow, purple or white
	Style     string  // progress, notched_6, notched_10, notched_12 or notched_20
	DarkenSky bool
	PlayMusic bool
	CreateFog bool
}

func liftDialogResponse(r types.DialogResponse) DialogResponse {
	return DialogResponse{
		Ctx:         liftContext(r.Ctx),
		Action:      r.Action,
		ValuesJSON:  r.ValuesJSON,
		PayloadJSON: liftOpt(r.PayloadJSON),
	}
}

func liftContext(c types.Context) Context {
	return Context{
		ConnectionID:  c.ConnectionID,
		UUID:          c.UUID,
		Name:          c.Name,
		Listener:      c.Listener,
		Hostname:      c.Hostname,
		PeerIP:        c.PeerIP,
		Protocol:      c.Protocol,
		Server:        liftOpt(c.Server),
		DefaultServer: liftOpt(c.DefaultServer),
		Target:        liftOpt(c.Target),
		Reason:        liftOpt(c.Reason),
		Success:       liftOpt(c.Success),
		Command:       liftOpt(c.Command),
		Args:          c.Args,
		Word:          c.Word,
		Position:      c.Position,
	}
}

func liftConnection(c types.Connection) Connection {
	return Connection{
		Listener:       c.Listener,
		PeerAddr:       c.PeerAddr,
		PeerIP:         c.PeerIP,
		PeerPort:       c.PeerPort,
		LocalAddr:      c.LocalAddr,
		LocalIP:        c.LocalIP,
		LocalPort:      c.LocalPort,
		DefaultBackend: liftOpt(c.DefaultBackend),
	}
}

func liftPing(p types.Ping) Ping {
	return Ping{
		Listener:    p.Listener,
		Hostname:    p.Hostname,
		Protocol:    p.Protocol,
		PeerIP:      p.PeerIP,
		Backend:     liftOpt(p.Backend),
		Maintenance: p.Maintenance,
		Online:      p.Online,
		MaxPlayers:  p.MaxPlayers,
	}
}

func liftHTTPRequest(r types.HTTPRequest) HTTPRequest {
	return HTTPRequest{
		Method:      r.Method,
		Path:        r.Path,
		Query:       r.Query,
		Body:        r.Body,
		Headers:     r.Headers.Slice(),
		ContextJSON: r.ContextJSON,
	}
}

func liftBusMessage(m types.BusMessage) BusMessage {
	return BusMessage{Handle: m.Handle, Subject: m.Subject, Payload: m.Payload.Slice(), Reply: liftOpt(m.Reply)}
}

func liftPlayerInfo(p types.PlayerInfo) PlayerInfo {
	return PlayerInfo{ConnectionID: p.ConnectionID, UUID: p.UUID, Name: p.Name, Server: p.Server, Protocol: p.Protocol}
}

func liftOpt[T any](o cm.Option[T]) *T {
	if v := o.Some(); v != nil {
		copied := *v
		return &copied
	}
	return nil
}

func lowerOpt[T any](v *T) cm.Option[T] {
	if v == nil {
		return cm.None[T]()
	}
	return cm.Some(*v)
}

func lowerOptString(v *string) cm.Option[string] { return lowerOpt(v) }
