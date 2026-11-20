package rift

import (
	"fmt"

	"go.bytecodealliance.org/cm"

	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/background"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/bus"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/log"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/meta"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/permissions"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/players"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/servers"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/store"
	hosttext "github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/text"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/timers"
	"github.com/Jotrorox/rift/sdk/go/gen/rift/plugin/types"
)

// HostError is an error the host returned from a host call.
type HostError string

func (e HostError) Error() string { return string(e) }

func unwrap[Shape, T any](r cm.Result[Shape, T, string]) (T, error) {
	if err := r.Err(); err != nil {
		var zero T
		return zero, HostError(*err)
	}
	return *r.OK(), nil
}

func optional(s string) cm.Option[string] {
	if s == "" {
		return cm.None[string]()
	}
	return cm.Some(s)
}

func optionalTime(unixSeconds uint64) cm.Option[uint64] {
	if unixSeconds == 0 {
		return cm.None[uint64]()
	}
	return cm.Some(unixSeconds)
}

// Players

// Players lists joined players, optionally only those on one server or
// service group ("" for all).
func Players(server string) ([]PlayerInfo, error) {
	list, err := unwrap(players.List(optional(server)))
	if err != nil {
		return nil, err
	}
	out := make([]PlayerInfo, 0, list.Len())
	for _, p := range list.Slice() {
		out = append(out, liftPlayerInfo(p))
	}
	return out, nil
}

// Player looks a player up by name, UUID or connection id; nil when offline.
func Player(target string) (*PlayerInfo, error) {
	found, err := unwrap(players.Get(target))
	if err != nil {
		return nil, err
	}
	if p := found.Some(); p != nil {
		info := liftPlayerInfo(*p)
		return &info, nil
	}
	return nil, nil
}

// Lookup returns what Rift remembers about a player who joined at least
// once, by UUID or by the name they last joined with (case-insensitive),
// online or not; nil for a player Rift never saw.
func Lookup(target string) (*PlayerRecord, error) {
	found, err := unwrap(players.Lookup(target))
	if err != nil {
		return nil, err
	}
	if r := found.Some(); r != nil {
		record := liftPlayerRecord(*r)
		return &record, nil
	}
	return nil, nil
}

// Search lists remembered players whose name starts with prefix, most
// recently seen first, at most limit (1..=100).
func Search(prefix string, limit uint32) ([]PlayerRecord, error) {
	list, err := unwrap(players.Search(prefix, limit))
	if err != nil {
		return nil, err
	}
	out := make([]PlayerRecord, 0, list.Len())
	for _, r := range list.Slice() {
		out = append(out, liftPlayerRecord(r))
	}
	return out, nil
}

// SetLanguage sets the language Rift's own messages use for a player, online
// or not: a tag of 2..=16 bytes of a-z0-9_, such as "de" or "de_de"; ""
// goes back to the client's locale. target is a UUID or a name Rift has
// seen. The language is remembered across sessions, so it is known in
// OnLogin. It reports whether the player is known.
func SetLanguage(target, language string) (bool, error) {
	return unwrap(players.SetLanguage(target, optional(language)))
}

// PlayerNames lists joined player names starting with prefix, sorted.
func PlayerNames(prefix string) []string {
	return players.Names(optional(prefix)).Slice()
}

// SendMessage sends a chat message to a player (name, UUID or connection
// id) and reports whether the player was online.
func SendMessage(target string, text Text) (bool, error) {
	return unwrap(players.Message(target, text.lower()))
}

// SendActionBar shows text above a player's hotbar.
func SendActionBar(target string, text Text) (bool, error) {
	return unwrap(players.ActionBar(target, text.lower()))
}

// SendTitle shows a title to a player:
//
//	rift.SendTitle(target, rift.Title{Title: rift.Mini("<gold>Welcome").Text(), FadeIn: 10, Stay: 70, FadeOut: 20})
func SendTitle(target string, t Title) (bool, error) {
	return unwrap(players.Title(target, types.TitleSpec{
		Title: t.Title.lowerOpt(), Subtitle: t.Subtitle.lowerOpt(), FadeIn: t.FadeIn, Stay: t.Stay, FadeOut: t.FadeOut,
	}))
}

// SetTabList sets a player's tab list header and footer; the zero Text
// leaves one out, and both zero restores the configured tab list.
func SetTabList(target string, header, footer Text) (bool, error) {
	return unwrap(players.TabList(target, header.lowerOpt(), footer.lowerOpt()))
}

// SetBossBar shows the boss bar called name to a player; nil removes it.
func SetBossBar(target, name string, bar *BossBar) (bool, error) {
	spec := cm.None[types.BossBarSpec]()
	if bar != nil {
		spec = cm.Some(types.BossBarSpec{
			Text: bar.Text.lower(), Progress: bar.Progress, Color: bar.Color, Style: bar.Style,
			DarkenSky: bar.DarkenSky, PlayMusic: bar.PlayMusic, CreateFog: bar.CreateFog,
		})
	}
	return unwrap(players.BossBar(target, name, spec))
}

// ShowDialog shows a player a dialog given as JSON in the vanilla dialog
// format (Minecraft 1.21.6+), replacing any open one. Buttons whose custom
// or dynamic/custom action has an id without a namespace come back to the
// OnDialog handler. It reports false for an offline or pre-1.21.6 player; an
// invalid dialog is an error.
func ShowDialog(target, dialogJSON string) (bool, error) {
	return unwrap(players.Dialog(target, dialogJSON))
}

// ClearDialog closes the dialog Rift showed the player, if it is still open.
func ClearDialog(target string) (bool, error) { return unwrap(players.ClearDialog(target)) }

// Kick disconnects a player with a reason.
func Kick(target string, reason Text) (bool, error) {
	return unwrap(players.Kick(target, reason.lower()))
}

// Transfer moves a player to a server.
func Transfer(target, server string) (bool, error) { return unwrap(players.Transfer(target, server)) }

// Broadcast messages every joined player and returns how many received it.
func Broadcast(text Text) (uint32, error) { return unwrap(players.Broadcast(text.lower())) }

// Store. Namespaces are prefixed with plugin.<name>. by the host.

// StoreGet reads a value; ok is false when the key is missing.
func StoreGet(namespace, key string) (value []byte, ok bool, err error) {
	found, err := unwrap(store.Get(namespace, key))
	if err != nil {
		return nil, false, err
	}
	if v := found.Some(); v != nil {
		return v.Slice(), true, nil
	}
	return nil, false, nil
}

// StoreSet writes a value.
func StoreSet(namespace, key string, value []byte) error {
	_, err := unwrap(store.Set(namespace, key, cm.ToList(value)))
	return err
}

// StoreDelete removes a value and reports whether it existed.
func StoreDelete(namespace, key string) (bool, error) { return unwrap(store.Delete(namespace, key)) }

// StoreIncrement adds delta to an integer entry (missing counts as 0) and
// returns the new value.
func StoreIncrement(namespace, key string, delta int64) (int64, error) {
	return unwrap(store.Increment(namespace, key, delta))
}

// StoreEntry is one key-value pair from StoreList.
type StoreEntry struct {
	Key   string
	Value []byte
}

// StoreList returns the entries whose key starts with prefix, sorted by key.
func StoreList(namespace, prefix string) ([]StoreEntry, error) {
	list, err := unwrap(store.List(namespace, prefix))
	if err != nil {
		return nil, err
	}
	out := make([]StoreEntry, 0, list.Len())
	for _, e := range list.Slice() {
		out = append(out, StoreEntry{Key: e.F0, Value: e.F1.Slice()})
	}
	return out, nil
}

// Permissions

// HasPermission reports whether a player (by UUID) has a permission node.
func HasPermission(uuid, node string) bool { return permissions.Has(uuid, node) }

// SetPermission grants (true) or denies (false) a node directly; nil removes
// the direct entry.
func SetPermission(uuid, node string, value *bool) error {
	_, err := unwrap(permissions.Set(uuid, node, lowerOpt(value)))
	return err
}

// AddGroup adds a player to a group, until expiresAt Unix seconds (0 for no
// expiry; a new expiry replaces an earlier one). It reports whether anything
// changed:
//
//	rift.AddGroup(uuid, "vip", uint64(rift.Time())+30*24*3600)
func AddGroup(uuid, group string, expiresAt uint64) (bool, error) {
	return unwrap(permissions.AddGroup(uuid, group, optionalTime(expiresAt)))
}

// RemoveGroup removes a player from a group and reports whether membership
// changed.
func RemoveGroup(uuid, group string) (bool, error) {
	return unwrap(permissions.RemoveGroup(uuid, group))
}

// Groups lists a player's groups, including inherited ones and default.
func Groups(uuid string) []string { return permissions.Groups(uuid).Slice() }

// ListGroupsJSON returns the configured groups and their permissions as a
// JSON object.
func ListGroupsJSON() string { return permissions.ListGroups() }

// VisibleToKey is the meta key that makes a player visible only to viewers
// holding the node in its value.
const VisibleToKey = "rift.visible-to"

// Meta returns one meta value in the player's current contexts, and false
// when it is unset.
func Meta(uuid, key string) (string, bool) {
	value := permissions.Meta(uuid, key)
	if found := value.Some(); found != nil {
		return *found, true
	}
	return "", false
}

// SetMeta sets the user's meta node key to value in the contexts of
// contextsJSON ("" for global), expiring at expiresAt Unix seconds (0 for
// never); remove deletes the node in those contexts instead. It reports
// whether anything changed.
func SetMeta(uuid, key, value string, remove bool, contextsJSON string, expiresAt uint64) (bool, error) {
	v := cm.Some(value)
	if remove {
		v = cm.None[string]()
	}
	return unwrap(permissions.SetMeta(uuid, key, v, optional(contextsJSON), optionalTime(expiresAt)))
}

// Bus

// Publish sends a message, with an optional reply subject ("" for none), and
// returns how many subscribers received it.
func Publish(subject string, payload []byte, reply string) (uint32, error) {
	return unwrap(bus.Publish(subject, cm.ToList(payload), optional(reply)))
}

// Reply publishes payload to msg's reply subject. It does nothing and
// returns false when msg has no reply subject.
func Reply(msg BusMessage, payload []byte) (bool, error) {
	if msg.Reply == nil {
		return false, nil
	}
	_, err := Publish(*msg.Reply, payload, "")
	return err == nil, err
}

// Request sends a request and waits up to timeoutMs for the first reply.
func Request(subject string, payload []byte, timeoutMs uint32) ([]byte, error) {
	reply, err := unwrap(bus.Request(subject, cm.ToList(payload), timeoutMs))
	if err != nil {
		return nil, err
	}
	return reply.Slice(), nil
}

// Subscribe delivers messages on subject to the OnMessage handler with the
// returned handle. A non-empty queueGroup load-balances between subscribers
// of the same group.
func Subscribe(subject, queueGroup string) (uint64, error) {
	return unwrap(bus.Subscribe(subject, optional(queueGroup)))
}

// Unsubscribe ends a subscription and reports whether it existed.
func Unsubscribe(handle uint64) bool { return bus.Unsubscribe(handle) }

// StreamPublish appends to a durable stream and returns the sequence number.
func StreamPublish(subject string, payload []byte) (uint64, error) {
	return unwrap(bus.StreamPublish(subject, cm.ToList(payload)))
}

// Managed servers. Results are the control service's JSON replies.

func ServerCreate(group string) (string, error)        { return unwrap(servers.Create(group)) }
func ServerRemove(name string) (string, error)         { return unwrap(servers.Remove(name)) }
func ServerStart(name string) (string, error)          { return unwrap(servers.Start(name)) }
func ServerStop(name string) (string, error)           { return unwrap(servers.Stop(name)) }
func TemplateDownload(template string) (string, error) { return unwrap(servers.Download(template)) }

// Control runs any other rift.control.<operation> with a JSON payload.
func Control(operation, payloadJSON string) (string, error) {
	return unwrap(servers.Control(operation, payloadJSON))
}

// Timers fire through the OnTimer handler with the returned handle.

// Every starts a timer repeating every ms milliseconds (at least 10).
func Every(ms uint32) uint64 { return timers.Every(ms) }

// After starts a timer firing once after ms milliseconds.
func After(ms uint32) uint64 { return timers.After(ms) }

// CancelTimer stops a timer and reports whether it existed.
func CancelTimer(handle uint64) bool { return timers.Cancel(handle) }

// Background work. A plugin runs one call at a time, so a slow lookup in a
// hook would keep every other hook waiting. Instead the hook defers its
// answer, queues the lookup for a background worker and returns; the OnJobDone
// handler answers with Resolve.

// Defer makes the running decision hook (login, initial-server,
// before-transfer, command, chat or kick) answer later: the handler's return
// value is ignored, and the answer is the one given to Resolve with the
// returned ticket. The hook's deadline keeps running. It is an error outside
// those hooks or when the call was deferred already.
func Defer() (ticket uint64, err error) { return unwrap(background.Defer()) }

// Resolve answers a deferred hook. It reports false when nobody waits for the
// ticket any more: the deadline passed, or it was answered.
func Resolve(ticket uint64, decision Decision) (bool, error) {
	return unwrap(background.Resolve(ticket, decision.lower()))
}

// Run queues payload for the plugin's background workers (manifest
// [background] workers; an error without any). One of them calls the OnWork
// handler with it, and the result comes back to this instance's OnJobDone
// handler with the returned handle. At most 1024 jobs wait at a time.
func Run(payload []byte) (handle uint64, err error) {
	return unwrap(background.Run(cm.ToList(payload)))
}

// IsWorker reports whether this instance is a background worker.
func IsWorker() bool { return background.IsWorker() }

// Logging, prefixed with the plugin name by the host.

// Log writes message at level.
func Log(level Level, message string) { log.Log(level, message) }

func Tracef(format string, args ...any) { log.Log(LevelTrace, fmt.Sprintf(format, args...)) }
func Debugf(format string, args ...any) { log.Log(LevelDebug, fmt.Sprintf(format, args...)) }
func Infof(format string, args ...any)  { log.Log(LevelInfo, fmt.Sprintf(format, args...)) }
func Warnf(format string, args ...any)  { log.Log(LevelWarn, fmt.Sprintf(format, args...)) }
func Errorf(format string, args ...any) { log.Log(LevelError, fmt.Sprintf(format, args...)) }

// Plugin metadata

// Name returns the plugin name from plugin.toml.
func Name() string { return meta.Name() }

// Version returns the plugin version from plugin.toml.
func Version() string { return meta.Version() }

// DataDir returns the guest path of the plugin's private data directory.
func DataDir() string { return meta.DataDir() }

// ConfigDir returns the guest path of the configuration directory, if granted.
func ConfigDir() (string, bool) {
	opt := meta.ConfigDir()
	if dir := opt.Some(); dir != nil {
		return *dir, true
	}
	return "", false
}

// Clock returns monotonic seconds since Rift started.
func Clock() float64 { return meta.Clock() }

// Time returns Unix time in seconds.
func Time() int64 { return meta.Time() }

// API returns the rift:plugin version the host implements, such as "0.3.0".
func API() string { return meta.API() }

// Text conversions

// RenderText returns the JSON text component t becomes, with markup, legacy
// codes and messages resolved, in language (such as "de" or "de_de"; "" for
// the default). Plain text becomes a JSON string. It also checks a template
// from the plugin's configuration during Init.
func RenderText(t Text, language string) (string, error) {
	return unwrap(hosttext.Render(t.lower(), optional(language)))
}

// PlainText returns the words of t without formatting, with messages
// resolved in language ("" for the default).
func PlainText(t Text, language string) (string, error) {
	return unwrap(hosttext.Plain(t.lower(), optional(language)))
}

// ToLegacy returns t with § codes, for surfaces that only take strings, with
// messages resolved in language ("" for the default).
func ToLegacy(t Text, language string) (string, error) {
	return unwrap(hosttext.ToLegacy(t.lower(), optional(language)))
}

// ToMini returns markup that writes t (plain text comes back escaped), with
// messages resolved in language ("" for the default).
func ToMini(t Text, language string) (string, error) {
	return unwrap(hosttext.ToMini(t.lower(), optional(language)))
}
