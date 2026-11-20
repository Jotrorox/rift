package rift

import (
	"go.bytecodealliance.org/cm"

	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/types"
)

// Hook names an event a plugin receives. List the hooks a plugin implements
// in Registration.Hooks. HookChat and HookKick are never implied by an empty
// list.
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
	HookChat           = types.HookChat
	HookKick           = types.HookKick
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
	Command       *string // command, suggest, and chat for a command: the command name
	Args          string  // the text after the command name
	Word          string  // suggest: the argument being completed
	Position      uint32  // suggest: its 1-based position
	// Language is the language set for the player with SetLanguage, known
	// from login on, else the client's locale once it is known.
	Language *string
	// Console is true when the operator console runs the command (command,
	// suggest): ConnectionID is 0, UUID is empty and Name is "CONSOLE". Only
	// commands registered with CommandSpec.Console get here.
	Console bool
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

// Decision answers a decision hook. Each hook accepts a subset:
//   - login, before-transfer, chat: Continue, Deny
//   - initial-server, kick: Continue, Deny, Server
//   - command: Continue, Message, Server, Queue, LeaveQueue
type Decision struct {
	kind  uint8
	value string
	text  Text
}

const (
	decisionContinue uint8 = iota
	decisionDeny
	decisionServer
	decisionMessage
	decisionQueue
	decisionLeaveQueue
)

// Continue lets the event proceed unchanged.
func Continue() Decision { return Decision{kind: decisionContinue} }

// Deny refuses the login or transfer, drops the chat message or disconnects
// the kicked player, with a reason shown to the player.
func Deny(reason Text) Decision { return Decision{kind: decisionDeny, text: reason} }

// Server sends the player to a server.
func Server(name string) Decision { return Decision{kind: decisionServer, value: name} }

// Message replies to a command with a chat message.
func Message(text Text) Decision { return Decision{kind: decisionMessage, text: text} }

// Queue puts the player into a server's queue.
func Queue(server string) Decision { return Decision{kind: decisionQueue, value: server} }

// LeaveQueue removes the player from their queue.
func LeaveQueue() Decision { return Decision{kind: decisionLeaveQueue} }

func (d Decision) lower() types.Decision {
	switch d.kind {
	case decisionDeny:
		return types.DecisionDeny(d.text.lower())
	case decisionServer:
		return types.DecisionServer(d.value)
	case decisionMessage:
		return types.DecisionMessage(d.text.lower())
	case decisionQueue:
		return types.DecisionQueue(d.value)
	case decisionLeaveQueue:
		return types.DecisionLeaveQueue()
	default:
		return types.DecisionContinue()
	}
}

// Suggestion is one completion offered by the suggest hook.
type Suggestion struct {
	Text    string
	Tooltip Text // the zero Text for no tooltip
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
	Name string // 1..=64 bytes of a-z0-9_-; "server" and "hub" are reserved
	// Aliases are other names for the command, each like Name, at most 16.
	// The hooks always see Name in Context.Command.
	Aliases    []string
	Permission string // the node a player needs to see and run the command; empty for every player
	Suggest    SuggestKind
	// Console lets operators run the command from the console (rift admin
	// plugins run); the hook then sees Context.Console.
	Console bool
}

// Registration is what the init hook returns.
type Registration struct {
	Commands []CommandSpec
	// Hooks to deliver. An empty list delivers every hook except HookChat
	// and HookKick; nil lets the SDK list the hooks that have a handler. The
	// dialog, menu and job-done handlers need no entry.
	Hooks []Hook
}

func (r Registration) lower() types.Registration {
	commands := make([]types.CommandSpec, len(r.Commands))
	for i, c := range r.Commands {
		commands[i] = types.CommandSpec{
			Name: c.Name, Aliases: cm.ToList(c.Aliases), Permission: c.Permission,
			Suggest: c.Suggest.lower(), Console: c.Console,
		}
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

// StatusOverride replaces fields of one ping response; nil fields and zero
// Texts keep the configured value. Clients show the version and the sample
// lines as § codes.
type StatusOverride struct {
	Motd       Text
	Version    Text
	Protocol   *int32
	Online     *uint32
	MaxPlayers *uint32
	Sample     []Text  // nil keeps the sample; use an empty slice to clear it
	Favicon    *string // a data:image/png;base64,... URI
}

func (s StatusOverride) lower() types.StatusOverride {
	out := types.StatusOverride{
		Motd:       s.Motd.lowerOpt(),
		Version:    s.Version.lowerOpt(),
		Protocol:   lowerOpt(s.Protocol),
		Online:     lowerOpt(s.Online),
		MaxPlayers: lowerOpt(s.MaxPlayers),
		Favicon:    lowerOptString(s.Favicon),
	}
	if s.Sample != nil {
		sample := make([]types.Text, len(s.Sample))
		for i, line := range s.Sample {
			sample[i] = line.lower()
		}
		out.Sample = cm.Some(cm.ToList(sample))
	}
	return out
}

// HTTPRequest is an authenticated request to /ext or /ext/* on the web server.
type HTTPRequest struct {
	Method      string
	Path        string
	Query       string
	Body        string
	Headers     [][2]string
	ContextJSON string // the status object plus "operator", the authenticated caller, as a JSON object
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

// ChatKind tells a chat message from a command.
type ChatKind = types.ChatKind

const (
	// ChatMessage is a chat message.
	ChatMessage = types.ChatKindMessage
	// ChatCommand is a command that neither Rift nor a plugin owns, on its
	// way to the backend.
	ChatCommand = types.ChatKindCommand
)

// ChatAttempt is a chat message or backend command a player sent, before
// the backend sees it: the input of the OnChat handler.
type ChatAttempt struct {
	// Ctx is the player; Server is their current backend. For a command,
	// Command is the root name and Args the rest.
	Ctx  Context
	Kind ChatKind
	// Text is the message, or the command line without the leading slash.
	Text string
}

// Event mirrors a rift.events.* publication.
type Event struct {
	Subject     string // for example rift.events.players
	PayloadJSON string // the event body as a JSON object
}

// ProfileProperty is one property of a player's profile, as the session
// server signed it.
type ProfileProperty struct {
	Name      string // for example "textures", the skin and cape
	Value     string
	Signature *string
}

// PlayerInfo is a joined player in the process-wide directory.
type PlayerInfo struct {
	ConnectionID uint64
	UUID         string
	Name         string
	Server       string // the backend the player is connected to
	Protocol     int32
	IP           string // the address the player connected from, without the port
	Properties   []ProfileProperty
	// VisibleTo is the permission node a viewer needs to see the player (their
	// rift.visible-to meta), or nil when everyone sees them. Players and
	// Player return hidden players too.
	VisibleTo *string
	// Locale is the language the player's client reported, as the client
	// spells it ("en_us"), or nil while Rift does not know it: the client
	// sends it after login.
	Locale *string
	// Language is the language Rift's own messages use for the player: the
	// one set with SetLanguage, else Locale.
	Language *string
}

// PlayerRecord is what Rift remembers about a player who joined at least
// once, online or not.
type PlayerRecord struct {
	UUID       string
	Name       string // the name the player last joined with
	FirstSeen  uint64 // Unix seconds
	LastSeen   uint64 // Unix seconds: the last join or quit
	LastIP     string // empty when players.record_ip is off
	LastServer string
	Online     bool
	Language   *string // the language set with SetLanguage, if any
}

// Title is a title shown to one player; times are ticks. Set at least one of
// Title and Subtitle; without Title only the subtitle is shown.
type Title struct {
	Title    Text
	Subtitle Text
	FadeIn   int32
	Stay     int32
	FadeOut  int32
}

// BossBar is a boss bar shown to one player.
type BossBar struct {
	Text      Text
	Progress  float32 // 0.0 to 1.0
	Color     string  // pink, blue, red, green, yellow, purple or white
	Style     string  // progress, notched_6, notched_10, notched_12 or notched_20
	DarkenSky bool
	PlayMusic bool
	CreateFog bool
}

func liftChatAttempt(a types.ChatAttempt) ChatAttempt {
	return ChatAttempt{Ctx: liftContext(a.Ctx), Kind: a.Kind, Text: a.Text}
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
		Language:      liftOpt(c.Language),
		Console:       c.Console,
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
	properties := make([]ProfileProperty, 0, p.Properties.Len())
	for _, prop := range p.Properties.Slice() {
		properties = append(properties, ProfileProperty{Name: prop.Name, Value: prop.Value, Signature: liftOpt(prop.Signature)})
	}
	return PlayerInfo{
		ConnectionID: p.ConnectionID, UUID: p.UUID, Name: p.Name, Server: p.Server, Protocol: p.Protocol,
		IP: p.IP, Properties: properties,
		VisibleTo: liftOpt(p.VisibleTo), Locale: liftOpt(p.Locale), Language: liftOpt(p.Language),
	}
}

func liftPlayerRecord(r types.PlayerRecord) PlayerRecord {
	return PlayerRecord{
		UUID: r.UUID, Name: r.Name, FirstSeen: r.FirstSeen, LastSeen: r.LastSeen,
		LastIP: r.LastIP, LastServer: r.LastServer, Online: r.Online, Language: liftOpt(r.Language),
	}
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
