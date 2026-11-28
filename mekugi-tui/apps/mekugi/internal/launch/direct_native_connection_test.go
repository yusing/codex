package launch

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json/jsontext"
	json "encoding/json/v2"
	"errors"
	"fmt"
	"image"
	"image/png"
	"maps"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/terminalsession"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/appserver"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffmodel"
	activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/terminalview"
	changes "github.com/mekugi-harness/mekugi/mekugi-tui/modules/changehistory/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/completion"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/composer"
	conversation "github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/prompt"
)

type mockNativeAppServer struct {
	mu           sync.Mutex
	conn         *websocket.Conn
	methods      []string
	params       map[string][]jsontext.Value
	responses    map[string]jsontext.Value
	overrides    map[string]func(jsontext.Value) any
	queueData    []map[string]any
	bgTerminals  []map[string]any
	shellFailMsg string
	ready        chan struct{}
}

func startMockNativeAppServer(t *testing.T, socket string) *mockNativeAppServer {
	t.Helper()
	listener, err := listenPrivateUnix(socket)
	if err != nil {
		t.Fatal(err)
	}
	m := &mockNativeAppServer{
		params:    make(map[string][]jsontext.Value),
		responses: make(map[string]jsontext.Value),
		overrides: make(map[string]func(jsontext.Value) any),
		ready:     make(chan struct{}, 4),
	}
	srv := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.CloseNow()
		m.mu.Lock()
		m.conn = conn
		m.mu.Unlock()
		select {
		case m.ready <- struct{}{}:
		default:
		}
		for {
			_, data, err := conn.Read(r.Context())
			if err != nil {
				return
			}
			var msg appserver.Message
			if err := json.Unmarshal(data, &msg); err != nil {
				continue
			}
			if msg.Method == "" && len(msg.ID) > 0 {
				m.mu.Lock()
				m.responses[string(msg.ID)] = slices.Clone(msg.Result)
				m.mu.Unlock()
				continue
			}
			m.mu.Lock()
			m.methods = append(m.methods, msg.Method)
			m.params[msg.Method] = append(m.params[msg.Method], slices.Clone(msg.Params))
			m.mu.Unlock()
			if len(msg.ID) == 0 {
				continue
			}
			result := m.handleCall(msg.Method, msg.Params)
			if errMsg, ok := result.(mockRPCError); ok {
				result = &appserver.Error{Code: -32600, Message: string(errMsg)}
			}
			if refusal, ok := result.(*appserver.Error); ok {
				reply, _ := json.Marshal(map[string]any{
					"jsonrpc": "2.0",
					"id":      msg.ID,
					"error":   refusal,
				})
				_ = conn.Write(r.Context(), websocket.MessageText, reply)
				continue
			}
			reply, _ := json.Marshal(map[string]any{
				"jsonrpc": "2.0",
				"id":      msg.ID,
				"result":  result,
			})
			_ = conn.Write(r.Context(), websocket.MessageText, reply)
		}
	})}
	go func() { _ = srv.Serve(listener) }()
	t.Cleanup(func() {
		_ = srv.Close()
		_ = listener.Close()
	})
	return m
}

type mockRPCError string

func (m *mockNativeAppServer) handleCall(method string, params jsontext.Value) any {
	m.mu.Lock()
	override := m.overrides[method]
	m.mu.Unlock()
	if override != nil {
		return override(params)
	}
	switch method {
	case "initialize":
		return map[string]any{"userAgent": "mekugi-app-server/test"}
	case "thread/start":
		cwd := "/tmp/ws"
		var startParams struct {
			Cwd string `json:"cwd"`
		}
		if json.Unmarshal(params, &startParams) == nil && startParams.Cwd != "" {
			cwd = startParams.Cwd
		}
		return map[string]any{
			"thread":          map[string]any{"id": "thr-main", "cwd": cwd, "name": "Direct Session"},
			"model":           "gpt-5",
			"reasoningEffort": "medium",
			"serviceTier":     "standard",
		}
	case "thread/fork":
		m.mu.Lock()
		count := len(m.params["thread/fork"])
		m.mu.Unlock()
		id := "thr-side"
		if count > 1 {
			id = "thr-btw"
		}
		return map[string]any{
			"thread":          map[string]any{"id": id, "cwd": "/tmp/ws"},
			"model":           "gpt-5",
			"reasoningEffort": "medium",
		}
	case "thread/subtree/list":
		return map[string]any{
			"data": []map[string]any{
				{"id": "thr-main", "agentRole": "root", "status": map[string]any{"type": "idle"}},
				{
					"id": "thr-worker", "parentThreadId": "thr-main", "agentRole": "explorer",
					"source": map[string]any{"subAgent": map[string]any{"thread_spawn": map[string]any{
						"parent_thread_id": "thr-main", "depth": 1, "agent_path": "/root/worker",
					}}},
					"status": map[string]any{"type": "active"},
				},
			},
		}
	case "thread/changes/read":
		return map[string]any{
			"threadId":       "thr-main",
			"workspace":      "/tmp/ws",
			"totalAdditions": 2,
			"totalDeletions": 1,
			"changes": []map[string]any{
				{"changeId": "amber1", "sequence": 1, "threadId": "thr-main",
					"authorThreadId": "thr-main", "turnId": "turn-1", "itemId": "captured-command",
					"sourceKind": "ShellWrite", "sourceLabel": "Shell", "workspace": "/tmp/ws"},
			},
			"files": []map[string]any{
				{"path": "main.go", "absolutePath": "/tmp/ws/main.go", "changeIds": []string{"amber1"},
					"status": "M", "unifiedDiff": "@@ -1 +1,2 @@\n-old\n+new\n+more\n", "additions": 2, "deletions": 1},
			},
		}
	case "thread/queue/list":
		m.mu.Lock()
		data := slices.Clone(m.queueData)
		m.mu.Unlock()
		if data == nil {
			data = []map[string]any{}
		}
		return map[string]any{"data": data, "nextCursor": nil}
	case "account/read":
		return map[string]any{
			"account": map[string]any{
				"type":     "chatgpt",
				"email":    "dev@example.com",
				"planType": "pro",
			},
		}
	case "account/rateLimits/read":
		return map[string]any{
			"rateLimits": map[string]any{
				"limitId":   "codex",
				"primary":   map[string]any{"usedPercent": 12.0, "windowDurationMins": 300},
				"secondary": map[string]any{"usedPercent": 21.0, "windowDurationMins": 10080},
			},
		}
	case "thread/backgroundTerminals/list":
		var req struct {
			ThreadID string `json:"threadId"`
		}
		_ = json.Unmarshal(params, &req)
		m.mu.Lock()
		procs := make([]map[string]any, 0, len(m.bgTerminals))
		for _, process := range m.bgTerminals {
			if process["threadId"] == req.ThreadID {
				procs = append(procs, process)
			}
		}
		m.mu.Unlock()
		return map[string]any{"data": procs, "nextCursor": nil}
	case "thread/backgroundTerminals/terminate":
		var req struct {
			ThreadID   string `json:"threadId"`
			ProcessID  string `json:"processId"`
			Invocation struct {
				TurnID string `json:"turnId"`
				ItemID string `json:"itemId"`
			} `json:"invocation"`
		}
		_ = json.Unmarshal(params, &req)
		m.mu.Lock()
		originalCount := len(m.bgTerminals)
		m.bgTerminals = slices.DeleteFunc(m.bgTerminals, func(p map[string]any) bool {
			return p["processId"] == req.ProcessID && p["threadId"] == req.ThreadID &&
				p["turnId"] == req.Invocation.TurnID && p["itemId"] == req.Invocation.ItemID
		})
		terminated := len(m.bgTerminals) < originalCount
		m.mu.Unlock()
		return map[string]any{"terminated": terminated}
	case "thread/shellCommand":
		m.mu.Lock()
		msg := m.shellFailMsg
		m.mu.Unlock()
		if msg != "" {
			return mockRPCError(msg)
		}
		return map[string]any{}
	case "thread/turns/list":
		var req struct {
			Cursor string `json:"cursor"`
		}
		_ = json.Unmarshal(params, &req)
		if req.Cursor == "older-rewind-turns" {
			return map[string]any{"data": []map[string]any{{
				"id": "turn-older", "status": "completed",
				"items": []map[string]any{
					{"id": "older", "type": "userMessage", "content": []map[string]string{{"type": "text", "text": "Older user message"}}},
				},
			}}}
		}
		return map[string]any{"data": []map[string]any{{
			"id": "turn-1", "status": "completed",
			"items": []map[string]any{
				{"id": "first", "type": "userMessage", "content": []map[string]string{{"type": "text", "text": "First user message"}}},
				{"id": "steered", "type": "userMessage", "content": []map[string]string{{"type": "text", "text": "Steered user message"}}},
			},
		}}, "nextCursor": "older-rewind-turns"}
	case "thread/revert/preview":
		return map[string]any{"changeIds": []string{"shell1"}}
	case "thread/read":
		return map[string]any{
			"thread": map[string]any{
				"id": "thr-main",
				"turns": []map[string]any{
					{
						"id": "turn-1",
						"items": []map[string]any{
							{"id": "cmd-1", "type": "commandExecution", "command": "echo hi", "status": "completed", "aggregatedOutput": "hi\n"},
							{"id": "card-788", "type": "serverInlineCard", "title": "Server Card", "body": "Rendered directly from native app-server", "status": "completed"},
						},
					},
				},
			},
		}
	case "thread/list":
		return map[string]any{
			"data": []map[string]any{
				{"id": "thr-saved", "name": "Saved session", "cwd": "/tmp/ws", "updatedAt": 1700000001, "gitInfo": map[string]any{"branch": "fix/saved"}},
			},
		}
	case "skills/list":
		return map[string]any{
			"data": []map[string]any{
				{
					"cwd": "/tmp/ws",
					"skills": []map[string]any{
						{"name": "review-pr", "path": "/tmp/ws/.agents/skills/review-pr/SKILL.md", "shortDescription": "Review a pull request", "enabled": true},
						{"name": "disabled-skill", "path": "/tmp/ws/.agents/skills/disabled/SKILL.md", "description": "Disabled", "enabled": false},
					},
				},
			},
		}
	case "thread/files/search":
		return map[string]any{"paths": []string{"cmd/main.go"}}
	case "model/list":
		return map[string]any{
			"data": []map[string]any{
				{
					"id":          "gpt-6.2-sol",
					"model":       "gpt-6.2-sol",
					"displayName": "GPT-6.2 Sol",
					"description": "Fast coding model",
					"supportedReasoningEfforts": []map[string]any{
						{"reasoningEffort": "low", "description": "Quick"},
						{"reasoningEffort": "high", "description": "Deep"},
					},
					"serviceTiers": []map[string]any{
						{"id": "priority", "name": "Fast", "description": "Priority tier"},
					},
				},
				{
					"id":           "gpt-5-standard",
					"model":        "gpt-5-standard",
					"displayName":  "GPT-5 Standard",
					"description":  "Standard-only model",
					"serviceTiers": []map[string]any{},
				},
				{
					"id":          "hidden-model",
					"model":       "hidden-model",
					"displayName": "Hidden",
					"hidden":      true,
				},
			},
		}
	default:
		return map[string]any{}
	}
}

func (m *mockNativeAppServer) send(t *testing.T, payload any) {
	t.Helper()
	m.mu.Lock()
	conn := m.conn
	m.mu.Unlock()
	data, err := json.Marshal(payload)
	if err != nil {
		t.Fatal(err)
	}
	if err := conn.Write(t.Context(), websocket.MessageText, data); err != nil {
		t.Fatal(err)
	}
}

func awaitDirectEvent(t *testing.T, conn *directNativeConnection, match func(sessionapi.NativeEvent) bool) sessionapi.NativeEvent {
	t.Helper()
	deadline := time.After(5 * time.Second)
	for {
		ch := make(chan sessionapi.NativeEvent, 1)
		errCh := make(chan error, 1)
		go func() {
			ev, err := conn.Next()
			if err != nil {
				errCh <- err
				return
			}
			ch <- ev
		}()
		select {
		case ev := <-ch:
			if match(ev) {
				return ev
			}
		case err := <-errCh:
			t.Fatalf("unexpected stream error: %v", err)
		case <-deadline:
			t.Fatal("timed out waiting for direct native event")
		}
	}
}

func TestDirectNativeConnectionEndToEnd(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)

	ctx, cancel := context.WithTimeout(t.Context(), 10*time.Second)
	defer cancel()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, []string{"--yolo", "-p", "custom-prof"}, "", "custom-prof")
	if err != nil {
		t.Fatalf("openDirectNativeConnection: %v", err)
	}
	defer conn.Close()

	// 1. Initial frame carries native thread, roster/family, and saved changes.
	initial := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Main.ID == "thr-main" &&
			ev.Frame.Family != nil && len(ev.Frame.Family.Threads) == 2 &&
			ev.Frame.Review != nil && len(ev.Frame.Review.Files) == 1
	})
	if initial.Frame.Review.Files[0].Path != "/tmp/ws/main.go" {
		t.Fatalf("unexpected saved change file: %+v", initial.Frame.Review.Files)
	}
	file := initial.Frame.Review.Unfiltered[initial.Frame.Review.Files[0].Key()]
	if len(file.Origins) != 1 || file.Origins[0].Caller != "/root" ||
		file.Origins[0].Change != "amber1" || file.Origins[0].Source != "Shell" {
		t.Fatalf("unexpected saved change provenance: %+v", file.Origins)
	}
	if counts := file.NetCounts(); counts.Added != 2 || counts.Removed != 1 {
		t.Fatalf("unexpected saved change counts: %+v", counts)
	}
	if initial.Frame.Main != initial.Frame.Agents {
		t.Fatalf("expected Main and Agents to share publication when Displayed == Main")
	}
	mock.mu.Lock()
	startParams := mock.params["thread/start"]
	mock.mu.Unlock()
	if len(startParams) != 1 {
		t.Fatalf("expected 1 thread/start call, got %d", len(startParams))
	}
	var startBody struct {
		Family map[string]jsontext.Value `json:"family"`
	}
	if err := json.Unmarshal(startParams[0], &startBody); err != nil {
		t.Fatalf("unmarshal thread/start: %v", err)
	}
	if _, hasHome := startBody.Family["home"]; hasHome {
		t.Fatalf("thread/start family must not send home: %s", startParams[0])
	}
	if len(startBody.Family["environment"]) == 0 || decodeJSONString(startBody.Family["profile"]) != "custom-prof" {
		t.Fatalf("thread/start family missing environment or profile: %s", startParams[0])
	}

	// 2. Live transcript items, command output hydration, and server-owned inline card (MEKUGI-788 style).
	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"method":  "turn/started",
		"params":  map[string]any{"threadId": "thr-main", "turn": map[string]any{"id": "turn-1"}},
	})
	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"method":  "item/started",
		"params": map[string]any{
			"threadId": "thr-main", "turnId": "turn-1",
			"item": map[string]any{"id": "cmd-1", "type": "commandExecution", "command": "echo hi", "status": "inProgress"},
		},
	})
	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"method":  "item/commandExecution/outputDelta",
		"params":  map[string]any{"threadId": "thr-main", "turnId": "turn-1", "itemId": "cmd-1", "delta": "hi\n"},
	})
	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"method":  "item/completed",
		"params": map[string]any{
			"threadId": "thr-main", "turnId": "turn-1",
			"item": map[string]any{
				"id":     "card-788",
				"type":   "serverInlineCard",
				"title":  "Server Card",
				"body":   "Rendered directly from native app-server",
				"status": "completed",
			},
		},
	})
	ev := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) >= 2
	})
	var cmdSeq uint64
	var foundCard bool
	for _, e := range ev.Frame.Main.Entries {
		if len(e.Values) > 0 && e.Values[0].Verb == "Run" {
			cmdSeq = e.Seq
		}
		if len(e.Values) > 0 && e.Values[0].Verb == "Server Card" && e.Text == "Rendered directly from native app-server" {
			foundCard = true
		}
	}
	if cmdSeq == 0 || !foundCard {
		t.Fatalf("expected command and server-owned inline card, got entries=%+v", ev.Frame.Main.Entries)
	}

	// Hydrate command output directly from native output buffer.
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Output: &sessionapi.NativeOutputDemand{Demands: []activity.OutputDemand{{Source: cmdSeq, Output: 1}}},
	}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return len(ev.Frame.MainOutput) == 1 && len(ev.Frame.MainOutput[0].View.Lines) > 0 && ev.Frame.MainOutput[0].View.Lines[0] == "hi"
	})

	// 3. Steering & queueing while turn is active (preserving image & selection attachments).
	steerDraft := composer.Draft{
		Text:         "steer [img] this",
		Images:       []composer.Token{{Start: 6, End: 11, Path: "/tmp/shot.png"}},
		AnswerImages: []string{"/tmp/ans.png"},
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Composer: &sessionapi.NativeComposerInput{
			Intent: sessionpresentation.ComposerIntent{Operation: sessionpresentation.SubmitComposer},
			Draft:  steerDraft,
		},
	}); err != nil {
		t.Fatal(err)
	}
	queueDraft := composer.Draft{
		Text:             "queue this",
		AnswerSelections: "selected snippet",
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Composer: &sessionapi.NativeComposerInput{
			Intent: sessionpresentation.ComposerIntent{Operation: sessionpresentation.QueueComposer},
			Draft:  queueDraft,
		},
	}); err != nil {
		t.Fatal(err)
	}

	// 4. Approvals (shellGuard with executable/argv, permissions) and secret prompts.
	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"id":      "req-approve-1",
		"method":  "shellGuard/requestApproval",
		"params": map[string]any{
			"threadId": "thr-main", "turnId": "turn-1", "itemId": "cmd-2",
			"kind": "remoteWrite", "cwd": "/tmp/ws", "executable": "git", "argv": []string{"push", "origin", "main"},
		},
	})
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return len(ev.Frame.Approvals) == 1 && ev.Frame.Approvals[0].ID == `"req-approve-1"`
	})
	if ev.Frame.Approvals[0].Subject != "git push origin main" {
		t.Fatalf("expected shellGuard approval subject to include executable+argv, got %q", ev.Frame.Approvals[0].Subject)
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Approval: &sessionpresentation.ApprovalIntent{Request: `"req-approve-1"`, Choice: 0},
	}); err != nil {
		t.Fatal(err)
	}

	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"id":      "req-perm-1",
		"method":  "item/permissions/requestApproval",
		"params": map[string]any{
			"threadId": "thr-main", "turnId": "turn-1", "itemId": "perm-1",
			"permissions": map[string]any{"network": map[string]any{"enabled": true}},
		},
	})
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return len(ev.Frame.Approvals) == 1 && ev.Frame.Approvals[0].ID == `"req-perm-1"`
	})
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Approval: &sessionpresentation.ApprovalIntent{Request: `"req-perm-1"`, Choice: 1},
	}); err != nil {
		t.Fatal(err)
	}

	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"id":      "req-secret-1",
		"method":  "shellGuard/requestSecret",
		"params":  map[string]any{"threadId": "thr-main", "kind": "sudoPassword", "executable": "sudo", "argv": []string{"apt", "update"}},
	})
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return len(ev.Frame.Questions) == 1 && ev.Frame.Questions[0].ID == `"req-secret-1"`
	})
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Answers: &sessionapi.NativeAnswers{
			Call:    `"req-secret-1"`,
			Answers: []sessionapi.NativeQuestionAnswer{{AnswerInput: prompt.AnswerInput{Question: "secret"}, Secret: "topsecret"}},
		},
	}); err != nil {
		t.Fatal(err)
	}
	mock.send(t, map[string]any{
		"jsonrpc": "2.0",
		"id":      "req-secret-2",
		"method":  "shellGuard/requestSecret",
		"params":  map[string]any{"threadId": "thr-main", "kind": "gpgPassphrase", "executable": "gpg", "argv": []string{"--sign"}},
	})
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return len(ev.Frame.Questions) == 1 && ev.Frame.Questions[0].ID == `"req-secret-2"`
	})
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Answers: &sessionapi.NativeAnswers{
			Call:       `"req-secret-2"`,
			Incomplete: true,
		},
	}); err != nil {
		t.Fatal(err)
	}

	// 5. Settings, rewind, one-off fork, side question, and Display selection.
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Command: new("/model gpt-5.1")}); err == nil {
		t.Fatal("/model accepted an argument instead of requiring the picker")
	}
	mock.send(t, map[string]any{
		"jsonrpc": "2.0", "method": "turn/completed",
		"params": map[string]any{"threadId": "thr-main", "turn": map[string]any{"id": "turn-1", "status": "completed"}},
	})
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && !ev.Frame.Facts.Busy
	})
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Command: new("/rewind")}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuRewind
	})
	menu := ev.Frame.View.Menu
	if len(menu.Choices) != 3 || menu.Choices[1].Name != "Steered user message" ||
		menu.Choices[2].Name != "Older messages…" {
		t.Fatalf("Rewind menu: %+v", menu)
	}
	mock.mu.Lock()
	rewindPageRequests := len(mock.params["thread/turns/list"])
	mock.mu.Unlock()
	if rewindPageRequests != 1 {
		t.Fatalf("Rewind fetched %d pages before opening", rewindPageRequests)
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[2].Identity(),
	}}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		next := ev.Frame.View.Menu
		return next != nil && next.Kind == sessionpresentation.MenuRewind && next.ID != menu.ID
	})
	menu = ev.Frame.View.Menu
	if len(menu.Choices) != 3 || menu.Choices[1].Name != "Steered user message" ||
		menu.Choices[2].Name != "Older user message" {
		t.Fatalf("Rewind menu after loading older messages: %+v", menu)
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[1].Identity(),
	}}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuRewindEdits
	})
	menu = ev.Frame.View.Menu
	if len(menu.Choices) != 2 || menu.Choices[0].Name != "Keep edits" || !menu.Choices[1].Danger {
		t.Fatalf("Rewind edits menu: %+v", menu)
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[0].Identity(),
	}}); err != nil {
		t.Fatal(err)
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Command: new("/rewind")}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuRewind
	})
	menu = ev.Frame.View.Menu
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[1].Identity(),
	}}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuRewindEdits
	})
	menu = ev.Frame.View.Menu
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[1].Identity(),
	}}); err != nil {
		t.Fatal(err)
	}
	for _, cmd := range []string{"/effort high", "/oneoff check this", "/btw quick question"} {
		if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Command: new(cmd)}); err != nil {
			t.Fatalf("command %q: %v", cmd, err)
		}
	}
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
		Display: &sessionpresentation.DisplayIntent{Thread: sessionpresentation.Thread{ID: "thr-worker", Caller: "main/worker"}},
	}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Displayed.ID == "thr-worker" && ev.Frame.View.Displayed.Caller == "main/worker"
	})

	// Verify history reload preserves visible transcript items and surviving Seq identities.
	conn.reloadThreadTranscript(ctx, "thr-main")
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 2
	})
	if ev.Frame.Main.Entries[0].Seq != cmdSeq {
		t.Fatalf("expected reload to preserve item Seq %d, got %d", cmdSeq, ev.Frame.Main.Entries[0].Seq)
	}

	// Verify resume picker scopes thread/list to current cwd.
	if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Command: new("/resume")}); err != nil {
		t.Fatal(err)
	}
	ev = awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Resume != nil })
	wantResume := sessionpresentation.ResumeRow{ID: "thr-saved", Title: "Saved session", Workspace: "/tmp/ws", Updated: 1700000001, Branch: "fix/saved"}
	if !slices.Equal(ev.Frame.View.Resume.Rows, []sessionpresentation.ResumeRow{wantResume}) {
		t.Fatalf("resume row lost native directory, age or branch: %+v", ev.Frame.View.Resume.Rows)
	}

	// Verify all native RPC methods were invoked directly on mekugi-app-server.
	mock.mu.Lock()
	methods := slices.Clone(mock.methods)
	approveResp := string(mock.responses[`"req-approve-1"`])
	permResp := string(mock.responses[`"req-perm-1"`])
	secretResp := string(mock.responses[`"req-secret-1"`])
	secretDeclineResp := string(mock.responses[`"req-secret-2"`])
	steerParams := slices.Clone(mock.params["turn/steer"])
	queueParams := slices.Clone(mock.params["thread/queue/add"])
	revertParams := slices.Clone(mock.params["thread/revert"])
	revertPreviewCount := len(mock.params["thread/revert/preview"])
	shellCount := len(mock.params["thread/shellCommand"])
	forkParams := slices.Clone(mock.params["thread/fork"])
	turnStartParams := slices.Clone(mock.params["turn/start"])
	listParams := slices.Clone(mock.params["thread/list"])
	turnsListParams := slices.Clone(mock.params["thread/turns/list"])
	mock.mu.Unlock()
	for _, want := range []string{"initialize", "thread/start", "thread/subtree/list", "thread/changes/read", "turn/steer", "thread/queue/add", "thread/settings/update", "thread/revert", "thread/fork", "turn/start", "thread/list"} {
		if !slices.Contains(methods, want) {
			t.Fatalf("missing direct native RPC %q; observed=%v", want, methods)
		}
	}
	if !strings.Contains(approveResp, `"decision":"accept"`) || strings.Contains(approveResp, `"allow"`) {
		t.Fatalf("unexpected shellGuard approval response: %s", approveResp)
	}
	if !strings.Contains(permResp, `"scope":"session"`) || !strings.Contains(permResp, `"network"`) {
		t.Fatalf("unexpected permissions approval response: %s", permResp)
	}
	if !strings.Contains(secretResp, `"decision":"provide"`) || !strings.Contains(secretResp, `"secret":"topsecret"`) {
		t.Fatalf("unexpected secret response: %s", secretResp)
	}
	if !strings.Contains(secretDeclineResp, `"decision":"decline"`) {
		t.Fatalf("unexpected declined secret response: %s", secretDeclineResp)
	}
	if len(steerParams) == 0 || !strings.Contains(string(steerParams[0]), "/tmp/shot.png") || !strings.Contains(string(steerParams[0]), "/tmp/ans.png") {
		t.Fatalf("turn/steer dropped image attachments: %s", steerParams)
	}
	if len(queueParams) == 0 || !strings.Contains(string(queueParams[0]), `"clientUserMessageId"`) || !strings.Contains(string(queueParams[0]), "selected snippet") {
		t.Fatalf("thread/queue/add missing clientUserMessageId or selection: %s", queueParams)
	}
	if len(revertParams) != 2 || !strings.Contains(string(revertParams[0]), `"beforeTurnId":"turn-1"`) ||
		!strings.Contains(string(revertParams[0]), `"beforeItemId":"steered"`) || strings.Contains(string(revertParams[0]), "expectedChangeIds") {
		t.Fatalf("thread/revert did not keep edits at the selected message: %s", revertParams)
	}
	if got, want := string(revertParams[1]), `{"threadId":"thr-main","beforeTurnId":"turn-1","beforeItemId":"steered","expectedChangeIds":["shell1"]}`; got != want {
		t.Fatalf("thread/revert did not send the selected preview IDs: %s", got)
	}
	if shellCount != 0 || revertPreviewCount != 2 {
		t.Fatalf("Rewind used %d shell commands and %d previews for two choices", shellCount, revertPreviewCount)
	}
	if len(forkParams) < 2 || !strings.Contains(string(forkParams[1]), `"sandbox":"read-only"`) || !strings.Contains(string(forkParams[1]), `"approvalPolicy":"never"`) {
		t.Fatalf("/btw fork missing read-only sandbox or approvalPolicy: %s", forkParams)
	}
	if len(turnStartParams) < 2 || !strings.Contains(string(turnStartParams[1]), `"environments":[]`) {
		t.Fatalf("/btw turn/start missing empty environments: %s", turnStartParams)
	}
	if len(listParams) == 0 || !strings.Contains(string(listParams[0]), `"cwd":"/tmp/ws"`) {
		t.Fatalf("thread/list missing cwd workspace filter: %s", listParams)
	}
	if len(turnsListParams) == 0 || !strings.Contains(string(turnsListParams[0]), `"sortDirection":"desc"`) {
		t.Fatalf("thread/turns/list missing sortDirection=desc: %s", turnsListParams)
	}
}

func TestDirectNativeResumeDoesNotWaitForSavedDiff(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	defer cancel()
	reviewStarted, releaseReview := make(chan struct{}), make(chan struct{})
	var started sync.Once
	mock.overrides["thread/resume"] = func(jsontext.Value) any {
		return map[string]any{"thread": map[string]any{"id": "thr-main", "cwd": "/tmp/ws"}}
	}
	mock.overrides["thread/changes/read"] = func(raw jsontext.Value) any {
		var params struct {
			Limit      int    `json:"limit"`
			Cursor     string `json:"cursor"`
			PerCapture bool   `json:"perCapture"`
		}
		if err := json.Unmarshal(raw, &params); err != nil || !params.PerCapture || params.Limit <= 0 {
			return mockRPCError("expected a bounded capture request")
		}
		switch params.Cursor {
		case "":
			started.Do(func() { close(reviewStarted) })
			select {
			case <-releaseReview:
			case <-ctx.Done():
				return mockRPCError("test canceled")
			}
			return map[string]any{
				"nextCursor": "1:0:20",
				"files": []map[string]any{{"path": "main.go", "absolutePath": "/tmp/ws/main.go",
					"changeIds": []string{"amber1"}, "status": "M", "unifiedDiff": "@@ -1 +1 @@\n-old\n"}},
			}
		case "1:0:20":
			return map[string]any{
				"changes": []map[string]any{{"changeId": "amber1", "sequence": 1, "threadId": "thr-main"}},
				"files": []map[string]any{{"path": "main.go", "absolutePath": "/tmp/ws/main.go",
					"changeIds": []string{"amber1"}, "status": "M", "unifiedDiff": "+new\n"}},
			}
		default:
			return mockRPCError("unexpected cursor")
		}
	}
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "thr-main", "")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	select {
	case <-reviewStarted:
	case <-ctx.Done():
		t.Fatal("no saved diff request while attached")
	}
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Main.ID == "thr-main" && ev.Frame.Review == nil
	})
	close(releaseReview)
	event := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Review != nil
	})
	if review := event.Frame.Review; len(review.Files) != 1 ||
		len(review.Files[0].Chunks) != 1 || review.Files[0].Chunks[0].Review.Diff != "@@ -1 +1 @@\n-old\n+new\n" {
		t.Fatalf("expected one reassembled capture: %+v", review)
	}
	mock.mu.Lock()
	mock.overrides["thread/changes/read"] = func(jsontext.Value) any {
		return mockRPCError("saved history unavailable")
	}
	mock.mu.Unlock()
	conn.requestChanges("thr-main")
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Review != nil && ev.Frame.Review.Failure == "saved history unavailable"
	})
	var alive directThreadStartResponse
	if err := conn.wire.Call(ctx, "thread/read", map[string]string{"threadId": "thr-main"}, &alive); err != nil {
		t.Fatalf("saved diff failure closed the attachment: %v", err)
	}
}

func TestDirectNativeReadChangesMergesOriginPathsAcrossPages(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	defer cancel()
	mock.overrides["thread/resume"] = func(jsontext.Value) any {
		return map[string]any{"thread": map[string]any{"id": "thr-main", "cwd": "/tmp/ws"}}
	}
	mock.overrides["thread/changes/read"] = func(raw jsontext.Value) any {
		var params struct {
			Cursor string `json:"cursor"`
		}
		if err := json.Unmarshal(raw, &params); err != nil {
			return mockRPCError("invalid params")
		}
		switch params.Cursor {
		case "":
			return map[string]any{
				"nextCursor": "1:1:524288",
				"changes": []map[string]any{{
					"changeId": "amber1", "sequence": 1, "threadId": "thr-main",
					"itemId": "cmd-1", "paths": []string{"a.go", "big.go"},
				}},
				"files": []map[string]any{
					{"path": "a.go", "absolutePath": "/tmp/ws/a.go", "changeIds": []string{"amber1"}, "status": "M", "unifiedDiff": "@@ -1 +1 @@\n-a\n+a2\n"},
					{"path": "big.go", "absolutePath": "/tmp/ws/big.go", "changeIds": []string{"amber1"}, "status": "M", "unifiedDiff": "@@ -1 +1 @@\n-old\n"},
				},
			}
		case "1:1:524288":
			return map[string]any{
				"changes": []map[string]any{{
					"changeId": "amber1", "sequence": 1, "threadId": "thr-main",
					"itemId": "cmd-1", "paths": []string{"big.go", "z.go"},
				}},
				"files": []map[string]any{
					{"path": "big.go", "absolutePath": "/tmp/ws/big.go", "changeIds": []string{"amber1"}, "status": "M", "unifiedDiff": "+new\n"},
					{"path": "z.go", "absolutePath": "/tmp/ws/z.go", "changeIds": []string{"amber1"}, "status": "A", "unifiedDiff": "@@ -0,0 +1 @@\n+z\n"},
				},
			}
		default:
			return mockRPCError("unexpected cursor")
		}
	}
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "thr-main", "")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	resp, err := conn.readChanges(ctx, "thr-main")
	if err != nil {
		t.Fatalf("readChanges failed: %v", err)
	}
	if len(resp.Changes) != 1 {
		t.Fatalf("expected 1 merged origin, got %d: %+v", len(resp.Changes), resp.Changes)
	}
	if want := []string{"a.go", "big.go", "z.go"}; !slices.Equal(resp.Changes[0].Paths, want) {
		t.Fatalf("origin.Paths = %v, want %v", resp.Changes[0].Paths, want)
	}
	if len(resp.Files) != 3 || resp.Files[1].UnifiedDiff != "@@ -1 +1 @@\n-old\n+new\n" {
		t.Fatalf("expected 3 files with reassembled big.go diff, got %+v", resp.Files)
	}
}

func TestMergeDirectChangeOriginLinearScalingAndFastPath(t *testing.T) {
	paths := make([]string, 5000)
	for i := range paths {
		paths[i] = fmt.Sprintf("pkg/file_%05d.go", i)
	}
	merged := mergeDirectChangeOrigin(directChangeOrigin{}, directChangeOrigin{
		ChangeID: "amber1",
		Sequence: 1,
		Paths:    paths,
	})
	// Repeated identical full-origin pages should hit the fast path without duplicating paths.
	for range 50 {
		merged = mergeDirectChangeOrigin(merged, directChangeOrigin{
			ChangeID: "amber1",
			Sequence: 1,
			Paths:    paths,
		})
	}
	if len(merged.Paths) != len(paths) {
		t.Fatalf("expected %d paths, got %d", len(paths), len(merged.Paths))
	}
	// Merging a differing page preserves order and appends only new unique paths.
	merged = mergeDirectChangeOrigin(merged, directChangeOrigin{
		ChangeID: "amber1",
		Paths:    []string{paths[0], "pkg/extra.go", "pkg/extra.go"},
	})
	if len(merged.Paths) != len(paths)+1 || merged.Paths[len(merged.Paths)-1] != "pkg/extra.go" {
		t.Fatalf("unexpected merged paths tail: len=%d last=%q", len(merged.Paths), merged.Paths[len(merged.Paths)-1])
	}
}

func TestDirectNativeNewClearsActiveTurnBeforeRewind(t *testing.T) {
	conn := &directNativeConnection{
		mainThread:    "thr-a",
		activeTurn:    "turn-a-1",
		threadCallers: map[string]string{"thr-a": "main"},
	}
	conn.applyThreadBootstrapLocked(directThreadStartResponse{
		Thread: directWireThread{ID: "thr-b", Cwd: "/tmp/ws", Name: "Fresh Session"},
		Model:  "gpt-5", ReasoningEffort: "medium", ServiceTier: "standard",
	})
	if conn.mainThread != "thr-b" {
		t.Fatalf("expected mainThread=thr-b, got %q", conn.mainThread)
	}
	if conn.activeTurn != "" {
		t.Fatalf("expected activeTurn cleared on main thread switch, got %q", conn.activeTurn)
	}
}

func TestDirectNativeForkPromptAndArchiveConfirmation(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)
	ctx := t.Context()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	send := func(op sessionapi.NativeOperation) {
		t.Helper()
		if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, op); err != nil {
			t.Fatal(err)
		}
	}
	requests := func(method string) []jsontext.Value {
		mock.mu.Lock()
		defer mock.mu.Unlock()
		return slices.Clone(mock.params[method])
	}
	send(sessionapi.NativeOperation{Command: new("/fork Continue in the branch")})
	prompts := requests("turn/start")
	if len(prompts) != 1 || !strings.Contains(string(prompts[0]), `"threadId":"thr-side"`) ||
		!strings.Contains(string(prompts[0]), `"text":"Continue in the branch"`) {
		t.Fatalf("expected prompt in returned fork, got %s", prompts)
	}
	mock.send(t, map[string]any{
		"jsonrpc": "2.0", "method": "turn/started",
		"params": map[string]any{"threadId": "thr-side", "turn": map[string]any{"id": "fork-turn"}},
	})
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool { return ev.Frame.Facts.Busy })
	open := func() *sessionpresentation.MenuView {
		t.Helper()
		send(sessionapi.NativeOperation{Command: new("/archive")})
		ev := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
			return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuArchive
		})
		return ev.Frame.View.Menu
	}
	pick := func(menu *sessionpresentation.MenuView, choice int) {
		send(sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
			Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[choice].Identity(),
		}})
	}
	menu := open()
	if !strings.Contains(menu.Explanation, "stop the current turn") {
		t.Fatalf("archive warning did not mention active work: %q", menu.Explanation)
	}
	pick(menu, 0)
	if archived := requests("thread/archive"); len(archived) != 0 {
		t.Fatalf("cancel sent an archive request: %s", archived)
	}
	pick(open(), 1)
	if archived := requests("thread/archive"); len(archived) != 1 || !strings.Contains(string(archived[0]), `"threadId":"thr-side"`) {
		t.Fatalf("expected archive of returned fork after confirmation, got %s", archived)
	}
}

func TestDirectNativeForkKeepsInheritedCommandDetailsThreadScoped(t *testing.T) {
	const main = "thr-main"
	const fork = "thr-fork"
	const turn = "turn-1"
	const run = "exec-12345678-1234-1234-1234-123456789abc"
	ctx := t.Context()
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)
	command := func(output string) map[string]any {
		return map[string]any{
			"type": "commandExecution", "id": run, "command": "echo inherited", "status": "completed",
			"exitCode": 0, "aggregatedOutput": output,
		}
	}
	retainedFork := func(output string) map[string]any {
		return map[string]any{"thread": map[string]any{
			"id": fork, "forkedFromId": main,
			"turns": []map[string]any{{"id": turn, "items": []map[string]any{command(output)}}},
		}}
	}
	mock.mu.Lock()
	mock.overrides["thread/fork"] = func(jsontext.Value) any { return retainedFork("first\nsecond\n") }
	mock.overrides["thread/read"] = func(jsontext.Value) any { return retainedFork("first\nsecond\nthird\n") }
	mock.mu.Unlock()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	send := func(op sessionapi.NativeOperation) {
		t.Helper()
		if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, op); err != nil {
			t.Fatal(err)
		}
	}
	mock.send(t, map[string]any{
		"jsonrpc": "2.0", "method": "item/completed",
		"params": map[string]any{"threadId": main, "turnId": turn, "item": command("first\n")},
	})
	sourceSeq := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return len(ev.Frame.Main.Entries) == 1
	}).Frame.Main.Entries[0].Seq

	send(sessionapi.NativeOperation{Command: new("/fork")})
	entries := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Main.ID == fork
	}).Frame.Main.Entries
	if len(entries) != 1 || !entries[0].Restored || entries[0].Seq == sourceSeq ||
		len(entries[0].Values) != 1 || entries[0].Values[0].Code != "echo inherited" {
		t.Fatalf("fork must publish its own inherited command: %+v", entries)
	}
	forkSeq := entries[0].Seq

	// Display refresh replaces this thread's retained records, not the source's.
	send(sessionapi.NativeOperation{Display: &sessionpresentation.DisplayIntent{
		Thread: sessionpresentation.Thread{ID: fork},
	}})
	reloaded := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		entries := ev.Frame.Main.Entries
		return len(entries) == 1 && len(entries[0].Values) == 1 &&
			slices.Contains(entries[0].Values[0].Tail, "third")
	}).Frame.Main.Entries[0]
	if reloaded.Seq != forkSeq || reloaded.Values[0].Source != forkSeq {
		t.Fatalf("reloaded fork changed its command's source: %+v", reloaded)
	}

	for _, tc := range []struct {
		thread string
		seq    uint64
		output []string
	}{
		{fork, forkSeq, []string{"first", "second", "third"}},
		{main, sourceSeq, []string{"first"}},
	} {
		send(sessionapi.NativeOperation{Details: &sessionapi.NativeDetailRequest{
			Kind: sessionapi.NativeRunDetails, Path: "mekugi:run/" + run,
			Displayed: sessionpresentation.Thread{ID: tc.thread},
		}})
		details := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
			return ev.Frame.Details != nil && ev.Frame.Details.Source.ID == tc.thread
		}).Frame.Details
		if len(details.View.Pages) != 1 || details.View.Pages[0].Source != tc.seq ||
			details.Output == nil || !details.Output.View.Exited || !slices.Equal(details.Output.View.Lines, tc.output) {
			t.Fatalf("resolved wrong run in %s: %+v", tc.thread, details)
		}
	}
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Details: &sessionapi.NativeDetailRequest{
			Kind: sessionapi.NativeEntryDetails, Source: sourceSeq,
			Displayed: sessionpresentation.Thread{ID: fork},
		},
	}); err == nil {
		t.Fatal("fork details accepted a source transcript row")
	}
}

func TestDirectNativeOneoffReplacementAndClear(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)
	ctx := t.Context()
	mock.mu.Lock()
	mock.overrides["thread/start"] = func(params jsontext.Value) any {
		id := "thr-main"
		if strings.Contains(string(params), `"sessionStartSource":"clear"`) {
			id = "thr-fresh"
		}
		return map[string]any{"thread": map[string]string{"id": id}, "reasoningEffort": "medium"}
	}
	mock.overrides["thread/fork"] = func(jsontext.Value) any {
		mock.mu.Lock()
		count := len(mock.params["thread/fork"])
		mock.mu.Unlock()
		if count == 1 {
			return jsontext.Value(`{"thread":{"id":"thr-side"},"model":"gpt-5","reasoningEffort":"xhigh"}`)
		}
		return jsontext.Value(`{"thread":{"id":"thr-btw"},"model":"chosen","reasoningEffort":"high"}`)
	}
	mock.overrides["thread/read"] = func(params jsontext.Value) any {
		return jsontext.Value(`{"thread":{"id":"thr-side","turns":[
			{"id":"parent-turn","items":[{"id":"old-command","type":"commandExecution","command":"echo inherited","status":"completed"}]},
			{"id":"side-turn","items":[{"id":"side-answer","type":"agentMessage","text":"One-off answer"}]}
		]}}`)
	}
	mock.mu.Unlock()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, []string{"--yolo"}, "", "")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	command := func(line string) error {
		_, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: &line})
		return err
	}
	if err := command("/oneoff"); err == nil {
		t.Fatal("/oneoff without a task succeeded")
	}
	if err := command("/effort xhigh"); err != nil {
		t.Fatal(err)
	}
	if err := command("/oneoff First task"); err != nil {
		t.Fatal(err)
	}
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Displayed.ID == "thr-side" && ev.Frame.View.Caption.Effort == "xhigh"
	})
	conn.reloadThreadTranscript(ctx, "thr-side")
	conn.mu.Lock()
	empty := len(conn.publicationForThreadLocked("thr-side").Entries) == 0
	conn.mu.Unlock()
	if !empty {
		t.Fatal("read before One-off starts displayed inherited turns")
	}
	conn.handleNotification(appserver.Message{Method: "thread/settings/updated", Params: jsontext.Value(`{
		"threadId":"thr-side","threadSettings":{"model":"gpt-6.2-sol","effort":"low"}
	}`)})
	if err := command("/effort"); err != nil {
		t.Fatal(err)
	}
	menu := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Setting == "/effort"
	}).Frame.View.Menu
	if menu.Preferred.Name != "low" {
		t.Fatalf("One-off effort picker ignored confirmed effort: %+v", menu)
	}
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: completion.ChoiceID{Name: "high"},
	}}); err != nil {
		t.Fatal(err)
	}
	conn.handleNotification(appserver.Message{Method: "thread/settings/updated", Params: jsontext.Value(`{
		"threadId":"thr-main","threadSettings":{"cwd":"/tmp/current","model":"chosen","modelProvider":"custom","effort":"high","serviceTier":"priority"}
	}`)})
	conn.handleNotification(appserver.Message{Method: "turn/started", Params: jsontext.Value(`{
		"threadId":"thr-side","turn":{"id":"side-turn"}
	}`)})
	conn.reloadThreadTranscript(ctx, "thr-side")
	conn.mu.Lock()
	entries := conn.publicationForThreadLocked("thr-side").Entries
	conn.mu.Unlock()
	if len(entries) != 1 || entries[0].Text != "One-off answer" {
		t.Fatalf("reattach displayed inherited Main history: %+v", entries)
	}
	for _, operation := range []sessionpresentation.ComposerOperation{
		sessionpresentation.SubmitComposer, sessionpresentation.QueueComposer,
	} {
		if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
			Composer: &sessionapi.NativeComposerInput{
				Draft:  composer.Draft{Text: "/compact"},
				Intent: sessionpresentation.ComposerIntent{Thread: "thr-side", Operation: operation},
			},
		}); err != nil {
			t.Fatal(err)
		}
	}
	if err := command("/clear"); err == nil {
		t.Fatal("/clear succeeded while One-off was working")
	}
	for _, confirm := range []bool{false, true} {
		if err := command("/oneoff Replacement task"); err != nil {
			t.Fatal(err)
		}
		menu := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
			return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuSideReplace
		}).Frame.View.Menu
		index := 0
		if confirm {
			index = 1
		}
		if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
			Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[index].Identity(),
		}}); err != nil {
			t.Fatal(err)
		}
	}
	if err := command("/clear"); err != nil {
		t.Fatal(err)
	}
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Main.ID == "thr-fresh" })
	mock.mu.Lock()
	defer mock.mu.Unlock()
	if forks := mock.params["thread/fork"]; len(forks) != 2 || !strings.Contains(string(forks[1]), `"threadId":"thr-main"`) {
		t.Fatalf("only confirmed replacement should fork Main again: %s", forks)
	} else if strings.Contains(string(forks[0]), `"model_reasoning_effort"`) {
		t.Fatalf("One-off should inherit native settings, not the stale client cache: %s", forks[0])
	}
	if archived := mock.params["thread/archive"]; len(archived) != 1 || !strings.Contains(string(archived[0]), `"threadId":"thr-side"`) {
		t.Fatalf("replacement did not archive the old One-off: %s", archived)
	}
	if compacts := mock.params["thread/compact/start"]; len(compacts) != 2 ||
		!strings.Contains(string(compacts[0]), `"mode":"continueInterrupted"`) ||
		!strings.Contains(string(compacts[0]), `"threadId":"thr-side"`) ||
		!strings.Contains(string(compacts[1]), `"mode":"queue"`) ||
		!strings.Contains(string(compacts[1]), `"threadId":"thr-side"`) {
		t.Fatalf("compaction did not distinguish Enter and Tab on One-off: %s", compacts)
	}
	if inputs := mock.params["turn/start"]; len(inputs) != 2 || !strings.Contains(string(inputs[1]), `"text":"Replacement task"`) {
		t.Fatalf("expected one replacement submission: %s", inputs)
	}
	if updates := mock.params["thread/settings/update"]; len(updates) != 2 ||
		!strings.Contains(string(updates[0]), `"threadId":"thr-main"`) || !strings.Contains(string(updates[0]), `"effort":"xhigh"`) ||
		!strings.Contains(string(updates[1]), `"threadId":"thr-side"`) || !strings.Contains(string(updates[1]), `"effort":"high"`) {
		t.Fatalf("Main and One-off effort settings did not target their own threads: %s", updates)
	}
	starts := mock.params["thread/start"]
	if len(starts) != 2 {
		t.Fatalf("only idle clear should start a thread: %s", starts)
	}
	for _, field := range []string{`"cwd":"/tmp/current"`, `"model":"chosen"`, `"modelProvider":"custom"`,
		`"model_reasoning_effort":"high"`, `"serviceTier":"priority"`, `"sessionStartSource":"clear"`, `"approvalPolicy":"never"`} {
		if !strings.Contains(string(starts[1]), field) {
			t.Fatalf("clear lost %s: %s", field, starts[1])
		}
	}
}

func TestDirectNativeFileCompletionBindsAndUndoes(t *testing.T) {
	workspace := t.TempDir()
	path := filepath.Join(workspace, "example")
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := errors.Join(png.Encode(file, image.NewRGBA(image.Rect(0, 0, 1, 1))), file.Close()); err != nil {
		t.Fatal(err)
	}
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)
	mock.mu.Lock()
	mock.overrides["thread/files/search"] = func(jsontext.Value) any {
		return map[string]any{"paths": []string{"example file.txt", "example"}}
	}
	mock.mu.Unlock()
	ctx := t.Context()
	conn, err := openDirectNativeConnection(ctx, socket, workspace, nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	for i, want := range []string{`see @"example file.txt" `, "see [Image 1] "} {
		var editor composer.Editor
		editor.Replace(composer.Draft{Text: "see @ex"})
		input := &sessionapi.NativeEditorInput{Target: conversation.EditorTarget{Revision: editor.Revision()}, Draft: editor.Draft}
		if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Completion: input}); err != nil {
			t.Fatal(err)
		}
		menu := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Menu != nil }).Frame.View.Menu
		if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
			Menu:   &sessionpresentation.MenuAction{Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[i].Identity()},
			Editor: input,
		}); err != nil {
			t.Fatal(err)
		}
		frame := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool { return len(ev.Frame.Effects) != 0 }).Frame
		effect := frame.Effects[0]
		if _, err := editor.Apply(*effect.Action); err != nil {
			t.Fatal(err)
		}
		if editor.Text != want || len(editor.Spans()) != 1 ||
			i == 0 && (len(editor.Files) != 1 || editor.Files[0].Path != "example file.txt") ||
			i == 1 && (len(editor.Images) != 1 || editor.Images[0].Path != path) {
			t.Fatalf("unexpected completed draft: %+v", editor.Draft)
		}
		editor.Undo(false)
		if editor.Text != "see @ex" || len(editor.Spans()) != 0 {
			t.Fatalf("one undo did not restore the original unbound draft: %+v", editor.Draft)
		}
		conn.handleReceipt(sessionapi.NativeReceipt{Effect: effect.ID})
	}
}

func TestDirectNativeUserMessageKeepsOnlyTypedSkillRange(t *testing.T) {
	text, spans := extractUserMessage(jsontext.Value(`[
		{"type":"text","text":"é $demo"},
		{"type":"text","text":"❯ $demo, $other","text_elements":[
			{"byteRange":{"start":4,"end":9},"placeholder":"$demo"},
			{"byteRange":{"start":4,"end":9},"placeholder":"$demo"},
			{"byteRange":{"start":11,"end":17},"placeholder":"$other"},
			{"byteRange":{"start":11,"end":99},"placeholder":"$demo"}
		]},
		{"type":"skill","name":"demo","path":"/workspace/.agents/skills/demo/SKILL.md"}
	]`))
	const prefix = "é $demo\n❯ "
	want := []activity.TextSpan{{Start: len(prefix), End: len(prefix + "$demo"), Kind: activity.SkillToken}}
	if text != prefix+"$demo, $other" || !slices.Equal(spans, want) {
		t.Fatalf("skill span projection = %q, %+v; want exact bound range %+v", text, spans, want)
	}
}

func TestDirectNativeUserMessageProjectsImageParts(t *testing.T) {
	text, spans := extractUserMessage(jsontext.Value(`[
		{"type":"text","text":"before "},
		{"type":"image","url":"opaque"},
		{"type":"text","text":" after; browser [Image photo.png]","text_elements":[
			{"byteRange":{"start":16,"end":33},"placeholder":"[Image photo.png]"}
		]},
		{"type":"localImage","path":"/workspace/photo.png"}
	]`))
	if text != "before [Image 1] after; browser [Image photo.png]" || len(spans) != 2 {
		t.Fatalf("image parts projected as %q with spans %+v", text, spans)
	}
	for i, want := range []string{"[Image 1]", "[Image photo.png]"} {
		span := spans[i]
		if span.Kind != activity.ImageToken || text[span.Start:span.End] != want {
			t.Fatalf("image span %d: %+v in %q", i, span, text)
		}
	}
}

func TestDirectNativeProjectionPopulatesEntryValues(t *testing.T) {
	conn := &directNativeConnection{
		wire:          &nativeAppServerWire{userAgent: "mekugi/0.172.0"},
		mainThread:    "thr-main",
		items:         make(map[string][]*directItemRecord),
		itemsByID:     make(map[directItemKey]*directItemRecord),
		threadCallers: map[string]string{"thr-main": "main"},
		events:        make(chan sessionapi.NativeEvent, 16),
	}

	conn.handleNotification(appserver.Message{
		Method: "item/agentMessage/delta",
		Params: jsontext.Value(`{"threadId":"thr-main","turnId":"turn-1","itemId":"msg-1","delta":"Hello "}`),
	})
	rec := conn.itemsByID[directItemKey{"thr-main", "msg-1"}]
	if rec == nil || len(rec.entry.Values) != 1 || rec.entry.Values[0].Kind != "text" || rec.entry.Values[0].Body != "Hello " {
		t.Fatalf("streaming agentMessage did not project Values: %+v", rec)
	}

	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{"threadId":"thr-main","turnId":"turn-1","item":{"type":"agentMessage","id":"msg-1","text":"Hello world"}}`),
	})
	if len(rec.entry.Values) != 1 || rec.entry.Values[0].Kind != "text" || rec.entry.Values[0].Body != "Hello world" || rec.entry.Agent != "Main" {
		t.Fatalf("completed agentMessage did not project Values: %+v", rec.entry.Values)
	}

	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{"threadId":"thr-main","turnId":"turn-1","item":{"type":"reasoning","id":"rsn-1","summary":["Thinking step"]}}`),
	})
	rsn := conn.itemsByID[directItemKey{"thr-main", "rsn-1"}]
	if rsn == nil || len(rsn.entry.Values) == 0 {
		t.Fatalf("reasoning item did not project Values: %+v", rsn)
	}

	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{"threadId":"thr-main","turnId":"turn-1","item":{"type":"plan","id":"pln-1","text":"1. Fix bug"}}`),
	})
	pln := conn.itemsByID[directItemKey{"thr-main", "pln-1"}]
	if pln == nil || len(pln.entry.Values) != 1 || pln.entry.Values[0].Body != "1. Fix bug" {
		t.Fatalf("plan item did not project Values: %+v", pln)
	}
}

func TestDirectNativeSlashCompletionAndMenuAccept(t *testing.T) {
	conn := &directNativeConnection{
		wire:          &nativeAppServerWire{userAgent: "mekugi/0.172.0"},
		mainThread:    "thr-main",
		items:         make(map[string][]*directItemRecord),
		itemsByID:     make(map[directItemKey]*directItemRecord),
		threadCallers: map[string]string{"thr-main": "main"},
		events:        make(chan sessionapi.NativeEvent, 16),
	}

	// Typing "/" opens slash command menu.
	_, err := conn.Send(context.Background(), sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Completion: &sessionapi.NativeEditorInput{
			Target: conversation.EditorTarget{Revision: 1},
			Draft:  composer.Draft{Text: "/"},
		},
	})
	if err != nil {
		t.Fatalf("completion failed: %v", err)
	}
	ev := <-conn.events
	if ev.Frame.View.Menu == nil || ev.Frame.View.Menu.Kind != sessionpresentation.MenuCompletion || len(ev.Frame.View.Menu.Choices) == 0 {
		t.Fatalf("expected slash completion menu, got %+v", ev.Frame.View.Menu)
	}
	if ev.Frame.CompletionTarget != (conversation.EditorTarget{Revision: 1}) {
		t.Fatalf("expected CompletionTarget revision 1 so renderer keeps menu visible, got %+v", ev.Frame.CompletionTarget)
	}
	menuID := ev.Frame.View.Menu.ID

	// Filtering "/res" ranks prefixes first, then by the matched span.
	_, _ = conn.Send(context.Background(), sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Completion: &sessionapi.NativeEditorInput{
			Target: conversation.EditorTarget{Revision: 2},
			Draft:  composer.Draft{Text: "/res"},
		},
	})
	ev = <-conn.events
	if menu := ev.Frame.View.Menu; menu == nil || !slices.EqualFunc(menu.Choices, []string{"/resume", "/reasoning", "/presets", "/predictions"}, func(choice completion.Choice, name string) bool {
		return choice.Name == name
	}) {
		t.Fatalf("expected the Go-era ranking for /res, got %+v", menu)
	}
	if ev.Frame.CompletionTarget != (conversation.EditorTarget{Revision: 2}) {
		t.Fatalf("expected CompletionTarget revision 2, got %+v", ev.Frame.CompletionTarget)
	}

	// Nonconsecutive fuzzy letters still find the command.
	_, _ = conn.Send(context.Background(), sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Completion: &sessionapi.NativeEditorInput{
			Target: conversation.EditorTarget{Revision: 3},
			Draft:  composer.Draft{Text: "/rsm"},
		},
	})
	ev = <-conn.events
	if menu := ev.Frame.View.Menu; menu == nil || len(menu.Choices) == 0 || menu.Choices[0].Name != "/resume" {
		t.Fatalf("expected fuzzy /rsm to select /resume, got %+v", menu)
	}

	// Accepting emits Delete + Insert effects targeting the editor revision sequence.
	_, _ = conn.Send(context.Background(), sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Menu: &sessionpresentation.MenuAction{
			Thread: "thr-main",
			Menu:   menuID,
			Kind:   sessionpresentation.MenuAccept,
			Choice: completion.ChoiceID{Name: "/resume"},
		},
		Editor: &sessionapi.NativeEditorInput{
			Target: conversation.EditorTarget{Revision: 3},
			Draft:  composer.Draft{Text: "/rsm"},
		},
	})
	ev = <-conn.events
	if ev.Frame.View.Menu != nil {
		t.Fatalf("expected menu closed after accept, got %+v", ev.Frame.View.Menu)
	}
	if len(ev.Frame.Effects) != 2 {
		t.Fatalf("expected 2 editor effects (delete+insert), got %+v", ev.Frame.Effects)
	}
	var ed composer.Editor
	ed.Replace(composer.Draft{Text: "/rsm"})
	for ed.Revision() < 3 {
		ed.Changed()
	}
	for _, eff := range ev.Frame.Effects {
		if eff.Target.Revision != ed.Revision() {
			t.Fatalf("effect target revision %d != editor revision %d", eff.Target.Revision, ed.Revision())
		}
		if _, err := ed.Apply(*eff.Action); err != nil {
			t.Fatalf("apply effect %+v: %v", eff.Action, err)
		}
	}
	if ed.Text != "/resume " {
		t.Fatalf("expected editor text with trailing space, got %q", ed.Text)
	}

	// Receipt prunes acknowledged effects.
	_, _ = conn.Send(context.Background(), sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Receipt: &sessionapi.NativeReceipt{Effect: ev.Frame.Effects[len(ev.Frame.Effects)-1].ID},
	})
	if len(conn.effects) != 0 {
		t.Fatalf("expected effects pruned on receipt, got %+v", conn.effects)
	}
}

func TestDirectNativeProjectionCommandFileChangeAndCtrlECollapse(t *testing.T) {
	conn := &directNativeConnection{
		wire:          &nativeAppServerWire{userAgent: "mekugi/0.172.0"},
		mainThread:    "thr-main",
		items:         make(map[string][]*directItemRecord),
		itemsByID:     make(map[directItemKey]*directItemRecord),
		threadCallers: map[string]string{"thr-main": "main"},
		events:        make(chan sessionapi.NativeEvent, 16),
	}

	conn.hydrateThreadTurnsLocked("thr-main", []directWireTurn{
		{
			ID: "turn-1",
			Items: []jsontext.Value{
				jsontext.Value(`{"id":"cmd-1","type":"commandExecution","command":"/bin/bash -lc 'printf \"a\\nb\\nc\\n\"'","display":{"script":"printf \"a\\nb\\nc\\n\"","segments":[],"effectiveCwd":null},"cwd":"/tmp/ws","status":"completed","exitCode":0,"durationMs":125,"aggregatedOutput":"a\nb\nc\n"}`),
				jsontext.Value("{\"id\":\"fc-1\",\"type\":\"fileChange\",\"status\":\"completed\",\"changes\":[{\"path\":\"a.go\",\"kind\":{\"type\":\"add\"},\"diff\":\"line1\\nline2\\n\"},{\"path\":\"b.go\",\"kind\":{\"type\":\"update\",\"move_path\":\"c.go\"},\"diff\":\"@@ -1 +1 @@\\n-old\\n+new\\n\"}]}"),
			},
		},
	})

	cmdRec := conn.itemsByID[directItemKey{"thr-main", "cmd-1"}]
	if cmdRec == nil || len(cmdRec.entry.Values) != 1 {
		t.Fatalf("expected projected commandExecution, got %+v", cmdRec)
	}
	val := cmdRec.entry.Values[0]
	if val.Lang != "bash" || val.Code != `printf "a\nb\nc\n"` {
		t.Fatalf("expected unwrapped bash command, got lang=%q code=%q", val.Lang, val.Code)
	}
	if !val.Exited || val.Running || val.ExitCode != 0 || len(val.Tail) != 3 || !cmdRec.entry.OutputComplete || !cmdRec.entry.Restored {
		t.Fatalf("expected completed restored Tail projection, got val=%+v entry=%+v", val, cmdRec.entry)
	}

	fcRec := conn.itemsByID[directItemKey{"thr-main", "fc-1"}]
	if fcRec == nil || len(fcRec.entry.Values) != 2 {
		t.Fatalf("expected 2 fileChange values, got %+v", fcRec)
	}
	if fcRec.entry.Values[0].Verb != "Create" || fcRec.entry.Values[0].Label != "`a.go` +2 -0" || fcRec.entry.Values[1].Verb != "Move" || fcRec.entry.Values[1].Label != "`b.go => c.go` +1 -1" {
		t.Fatalf("unexpected fileChange decoding: %+v", fcRec.entry.Values)
	}

	// Verify terminalview collapses restored multi-line command output by default and ctrl+e cycles Expansion.
	var tv terminalview.View
	tv.Update(&activity.Publication{Entries: []activity.Entry{cmdRec.entry}}, terminalview.Display{})
	if tv.Expansion != 0 {
		t.Fatalf("expected default Expansion=0, got %d", tv.Expansion)
	}
	tv.ToggleExpansion()
	if tv.Expansion != 1 {
		t.Fatalf("expected Expansion=1 after first ctrl+e, got %d", tv.Expansion)
	}
	tv.ToggleExpansion()
	if tv.Expansion != 2 {
		t.Fatalf("expected Expansion=2 after second ctrl+e, got %d", tv.Expansion)
	}
	tv.ToggleExpansion()
	if tv.Expansion != 0 {
		t.Fatalf("expected Expansion=0 after third ctrl+e, got %d", tv.Expansion)
	}
}

func TestDirectNativeCommandDetailsFollowNativeExit(t *testing.T) {
	conn := &directNativeConnection{
		wire:          &nativeAppServerWire{userAgent: "mekugi/0.172.0"},
		mainThread:    "thr-main",
		workspace:     "/tmp/ws",
		items:         make(map[string][]*directItemRecord),
		itemsByID:     make(map[directItemKey]*directItemRecord),
		threadCallers: map[string]string{"thr-main": "/root"},
		events:        make(chan sessionapi.NativeEvent, 16),
	}
	upsert := func(body string) *directItemRecord {
		conn.upsertWireItemLocked("thr-main", "turn-1", jsontext.Value(body), true)
		return conn.itemsByID[directItemKey{"thr-main", "cmd-1"}]
	}
	rec := upsert(`{"type":"commandExecution","id":"cmd-1","command":"cd sub && example","cwd":"/tmp/ws","display":{"script":"cd sub && example","segments":["example"],"effectiveCwd":"/tmp/ws/sub"},"status":"inProgress","processId":"35","exitCode":null,"aggregatedOutput":"starting\rworking\t25%\r\n"}`)
	if !rec.entry.ItemComplete || rec.entry.OutputComplete || rec.entry.Values[0].RowVerb() != "Running" {
		t.Fatalf("item completion must not settle a running command: %+v", rec.entry)
	}
	if value := rec.entry.Values[0]; !value.ShowWorkdir || value.Workdir != "/tmp/ws/sub" || value.Code != "example" {
		t.Fatalf("single command lost its effective directory: %+v", value)
	}
	if tail := rec.entry.Values[0].Tail; !slices.Equal(tail, []string{"working    25%"}) {
		t.Fatalf("command tail did not normalize progress output: %q", tail)
	}
	if _, err := conn.Send(t.Context(), sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Details: &sessionapi.NativeDetailRequest{Kind: sessionapi.NativeRunDetails, Source: rec.seq},
	}); err != nil {
		t.Fatal(err)
	}
	before := conn.details
	if before.Output == nil || before.Output.View.Done || before.Output.View.Exited {
		t.Fatalf("live output should have no exit: %+v", before)
	}
	if lines := before.Output.View.Lines; !slices.Equal(lines, []string{"working\t25%"}) {
		t.Fatalf("Details output should normalize progress without changing tabs: %q", lines)
	}
	upsert(`{"type":"commandExecution","id":"cmd-1","command":"example","status":"declined","exitCode":null}`)
	if conn.details.Output.View.Exited || conn.details.View.Pages[0].RowVerb() == "Ran" {
		t.Fatalf("approval row closure must not imply a process exit: %+v", conn.details)
	}
	upsert(`{"type":"commandExecution","id":"cmd-1","command":"example","status":"declined","exitCode":-1,"aggregatedOutput":"rejected by user"}`)
	got := conn.details
	if got.View.ID != before.View.ID || len(got.View.Pages) != 1 || !got.Output.View.Done ||
		!got.Output.View.Exited || got.Output.View.Exit != -1 ||
		!slices.Equal(got.Output.View.Lines, []string{"rejected by user"}) || before.Output.View.Done {
		t.Fatalf("the same output view should settle with the native exit and output: %+v", got.Output)
	}
}

func TestDirectNativeRetainedCommandEditReceipt(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "app-server.sock")
	mock := startMockNativeAppServer(t, socket)
	conn, err := openDirectNativeConnection(t.Context(), socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	mock.send(t, map[string]any{
		"jsonrpc": "2.0", "method": "item/completed",
		"params": map[string]any{
			"threadId": "thr-main", "turnId": "turn-1",
			"item": map[string]any{"type": "commandExecution", "id": "captured-command",
				"command": "update main.go", "status": "failed", "exitCode": 1,
				"aggregatedOutput": "updated\nthen failed\n"},
		},
	})
	ev := awaitDirectEvent(t, conn, func(event sessionapi.NativeEvent) bool {
		return event.Frame.Main != nil && len(event.Frame.Main.Entries) > 0 &&
			len(event.Frame.Main.Entries[0].Values[0].Changes) > 0
	})
	entry := ev.Frame.Main.Entries[0]
	rows := entry.Values[0].Changes
	if len(rows) != 1 || rows[0].Label != "main.go" || rows[0].Added != 2 ||
		rows[0].Removed != 1 || rows[0].Note != "Shell · amber1" {
		t.Fatalf("native execution did not retain its captured edit: %+v", rows)
	}
	if err := conn.handleOpenDetails(t.Context(), sessionapi.NativeDetailRequest{Source: entry.Seq}); err != nil {
		t.Fatal(err)
	}
	details := conn.details
	if len(details.View.Pages) != 2 || details.Output == nil ||
		!slices.Equal(details.Output.View.Lines, []string{"updated", "then failed"}) ||
		!strings.Contains(details.View.Pages[1].Code, "+more") ||
		details.View.Pages[1].Path != "/tmp/ws/main.go" {
		t.Fatalf("Details did not preserve one retained output and the actual edit: %+v", details)
	}
}

func TestDirectNativeRetainedRunLink(t *testing.T) {
	const run = "exec-12345678-1234-1234-1234-123456789abc"
	conn := &directNativeConnection{
		wire:       &nativeAppServerWire{userAgent: "mekugi/test"},
		mainThread: "thr-main",
		items:      make(map[string][]*directItemRecord),
		itemsByID:  make(map[directItemKey]*directItemRecord),
		events:     make(chan sessionapi.NativeEvent, 4),
	}
	conn.hydrateThreadTurnsLocked("thr-main", []directWireTurn{{
		ID:    "turn-1",
		Items: []jsontext.Value{jsontext.Value(`{"type":"commandExecution","id":"` + run + `","command":"/bin/bash -lc 'cd sub && cat a && cat b'","cwd":"/repo","commandActions":[{"type":"unknown","command":"cat"}],"display":{"script":"cd sub && cat a && cat b","effectiveCwd":"/repo/sub","segments":["cat a","cat b"],"stepKinds":["read","read"]},"status":"failed","exitCode":-1,"aggregatedOutput":"one\nold 25%\rtwo\r\nthree\n"}`)},
	}})
	batch := conn.itemsByID[directItemKey{"thr-main", run}].entry.Values
	if len(batch) != 3 || batch[0].Code != "cat a" || batch[1].Code != "cat b" ||
		batch[0].Workdir != "/repo/sub" || batch[1].Workdir != "/repo/sub" || batch[2].Workdir != "/repo/sub" ||
		batch[0].Exited || batch[1].Exited || batch[0].Output != 0 || batch[1].Output != 0 ||
		!batch[2].BatchExit || !batch[2].Exited || batch[2].ExitCode != -1 || batch[2].Output != 1 {
		t.Fatalf("expected classified steps with one native batch result, got %+v", batch)
	}
	visible := conn.publicationForThreadLocked("thr-main").Entries[0]
	if len(visible.Values) != 2 || visible.Values[0].Verb != "Read" || visible.Values[1].Verb != "Read" {
		t.Fatalf("transcript should show only the authored steps: %+v", visible.Values)
	}
	if err := conn.handleOpenDetails(t.Context(), sessionapi.NativeDetailRequest{
		Source: visible.Seq, Operation: visible.Values[1].Operation,
	}); err != nil {
		t.Fatal(err)
	}
	if out := conn.details.Output; out == nil || !out.View.Exited || out.View.Exit != -1 ||
		!slices.Equal(out.View.Lines, []string{"one", "two", "three"}) {
		t.Fatalf("step Details should open the normalized invocation output: %+v", out)
	}
	const editPath = "/repo/sub/b"
	const editDiff = "@@ -1 +1 @@\n-old\n+new\n"
	conn.editReceipts = map[nativeEditItem][]nativeCapturedFile{
		{"thr-main", "turn-1", run}: {{
			path: editPath,
			chunk: changes.Chunk{Workspace: "/repo", Review: diffmodel.ReviewFile{
				BeforePath: editPath, AfterPath: editPath, Diff: editDiff,
			}},
		}},
	}
	if err := conn.handleOpenDetails(t.Context(), sessionapi.NativeDetailRequest{
		Source: visible.Seq, Operation: visible.Values[1].Operation, Path: editPath, Edit: true,
	}); err != nil {
		t.Fatal(err)
	}
	if details := conn.details; len(details.View.Pages) != 4 || details.Output != nil ||
		details.Selected != details.View.Pages[3].ID || details.View.Pages[3].Code != editDiff {
		t.Fatalf("captured edit link should select the diff instead of batch output: %+v", details)
	}

	for _, tc := range []struct {
		link      string
		want      sessionpresentation.RunOutputRange
		displayed string
	}{
		{link: "#L2-L3", want: sessionpresentation.RunOutputRange{First: 2, Last: 3, Available: true}},
		{link: "#L2-L4", want: sessionpresentation.RunOutputRange{First: 2, Last: 4}},
		{link: "#L2", displayed: "thr-other"},
	} {
		err := conn.handleOpenDetails(t.Context(), sessionapi.NativeDetailRequest{
			Kind: sessionapi.NativeRunDetails, Path: "mekugi:run/" + run + tc.link,
			Displayed: sessionpresentation.Thread{ID: tc.displayed},
		})
		if tc.displayed != "" {
			if err == nil {
				t.Fatal("run link resolved against a different thread")
			}
			continue
		}
		if err != nil {
			t.Fatal(err)
		}
		if details := conn.details; details.RunRange == nil || *details.RunRange != tc.want ||
			details.View.Pages[0].DetailCode != "cd sub && cat a && cat b" ||
			details.Output == nil || !slices.Equal(details.Output.View.Lines, []string{"one", "two", "three"}) {
			t.Fatalf("%s opened wrong output or range: %+v", tc.link, details)
		}
	}
}

func TestDirectNativeQuitOffersToStopRunningWork(t *testing.T) {
	for _, tc := range []struct {
		name, kind string
		refusal    *appserver.Error
		wantError  bool
	}{
		{name: "continue main", kind: "main"},
		{name: "stop command", kind: "command"},
		{name: "completed delegate", kind: "delegate", refusal: &appserver.Error{
			Code: -32600, Message: "The turn has finished.", Data: jsontext.Value(`{"reason":"noActiveTurn"}`),
		}},
		{name: "replaced delegate", kind: "delegate", refusal: &appserver.Error{
			Code: -32600, Message: "A replacement is now active.",
			Data: jsontext.Value(`{"reason":"activeTurnMismatch","expectedTurnId":"turn-running","actualTurnId":"turn-replacement"}`),
		}},
		{name: "untyped refusal", kind: "delegate", wantError: true, refusal: &appserver.Error{
			Code: -32600, Message: "no active turn to interrupt",
		}},
		{name: "different interrupt target", kind: "delegate", wantError: true, refusal: &appserver.Error{
			Code: -32600, Message: "Different target.",
			Data: jsontext.Value(`{"reason":"activeTurnMismatch","expectedTurnId":"other","actualTurnId":"turn-replacement"}`),
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			kind := tc.kind
			socket := filepath.Join(shortTempDir(t), "native.sock")
			server := startMockNativeAppServer(t, socket)
			state := func(id string) map[string]any {
				status := "idle"
				if id == "thr-"+kind {
					status = "active"
				}
				return map[string]any{"id": id, "status": map[string]string{"type": status}}
			}
			server.mu.Lock()
			server.overrides["thread/subtree/list"] = func(jsontext.Value) any {
				return map[string]any{"data": []any{state("thr-main"), state("thr-delegate")}}
			}
			activeTurn := "turn-running"
			server.overrides["thread/turns/list"] = func(jsontext.Value) any {
				server.mu.Lock()
				defer server.mu.Unlock()
				return map[string]any{"data": []map[string]string{{"id": activeTurn, "status": "inProgress"}}}
			}
			if kind == "command" {
				server.bgTerminals = []map[string]any{{
					"threadId": "thr-delegate", "turnId": "turn-ended", "itemId": "cmd",
					"processId": "42", "command": "sleep 60",
				}}
			}
			server.mu.Unlock()
			ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
			defer cancel()
			conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
			if err != nil {
				t.Fatal(err)
			}
			defer conn.Close()
			if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/quit")}); err != nil {
				t.Fatal(err)
			}
			ev := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
				return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuExit
			})
			menu := ev.Frame.View.Menu
			if len(menu.Choices) != 2 || menu.Choices[0].Name != "Continue in background" ||
				menu.Choices[1].Name != "Stop" || !menu.Choices[1].Danger {
				t.Fatalf("quit choices: %+v", menu.Choices)
			}
			choose := func(index int) error {
				_, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
					Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[index].Identity(),
				}})
				return err
			}
			if tc.refusal != nil {
				server.mu.Lock()
				activeTurn = "turn-replacement"
				server.overrides["turn/interrupt"] = func(jsontext.Value) any {
					return tc.refusal
				}
				server.mu.Unlock()
			}
			var quitErr error
			if kind == "main" {
				quitErr = choose(0)
			} else {
				quitErr = choose(1)
			}
			if tc.wantError {
				if refusal, ok := errors.AsType[*nativeAppServerRPCError](quitErr); !ok || refusal.Message != tc.refusal.Message {
					t.Fatalf("Stop error: %v, want %q", quitErr, tc.refusal.Message)
				}
				return
			}
			if quitErr != nil {
				t.Fatal(quitErr)
			}
			select {
			case err := <-conn.errCh:
				var exit *sessionapi.NativeExit
				if !errors.As(err, &exit) || exit.Code != 0 {
					t.Fatalf("quit result: %v", err)
				}
			case <-ctx.Done():
				t.Fatal("confirmed quit did not close the connection")
			}
			server.mu.Lock()
			defer server.mu.Unlock()
			interrupts := server.params["turn/interrupt"]
			switch kind {
			case "main":
				if len(interrupts) != 0 {
					t.Fatalf("Continue interrupted work: %s", interrupts)
				}
			case "delegate":
				want := map[string]string{"threadId": "thr-delegate", "turnId": "turn-running"}
				var got map[string]string
				if len(interrupts) != 1 || json.Unmarshal(interrupts[0], &got) != nil || !maps.Equal(got, want) {
					t.Fatalf("Stop interrupt requests: %s, want only %v", interrupts, want)
				}
			case "command":
				if len(server.bgTerminals) != 0 || len(interrupts) != 0 {
					t.Fatalf("Stop left processes %v; turn interrupt requests %s", server.bgTerminals, interrupts)
				}
			}
		})
	}
}

func TestDirectNativeSkillFileCompletionsAndSettingsPickers(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "native.sock")
	srv := startMockNativeAppServer(t, socket)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatalf("openDirectNativeConnection: %v", err)
	}
	defer conn.Close()
	<-srv.ready

	drain := func() sessionapi.NativeEvent {
		var last sessionapi.NativeEvent
		for {
			select {
			case ev := <-conn.events:
				last = ev
			default:
				return last
			}
		}
	}
	_ = drain()

	// $ skill completion queries skills/list and filters enabled skills.
	var editor composer.Editor
	editor.Replace(composer.Draft{Text: "use $rev today", CursorBack: len(" today")})
	input := &sessionapi.NativeEditorInput{Target: conversation.EditorTarget{Revision: editor.Revision()}, Draft: editor.Draft}
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Completion: input,
	})
	if err != nil {
		t.Fatalf("$ completion: %v", err)
	}
	ev := drain()
	if ev.Frame.View.Menu == nil || len(ev.Frame.View.Menu.Choices) != 1 || ev.Frame.View.Menu.Choices[0].Name != "$review-pr" {
		t.Fatalf("expected $review-pr skill choice, got %+v", ev.Frame.View.Menu)
	}
	menu := ev.Frame.View.Menu
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Editor: input,
		Menu:   &sessionpresentation.MenuAction{Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[0].Identity()},
	}); err != nil {
		t.Fatal(err)
	}
	effects := drain().Frame.Effects
	if len(effects) != 1 || effects[0].Action == nil || effects[0].Target == nil || *effects[0].Target != input.Target {
		t.Fatalf("expected one revision-scoped completion action, got %+v", effects)
	}
	if _, err := editor.Apply(*effects[0].Action); err != nil {
		t.Fatal(err)
	}
	if editor.Text != "use $review-pr today" || editor.Cursor() != len("use $review-pr ") {
		t.Fatalf("completion should reuse the existing separator: %+v", editor.Draft)
	}
	editor.Undo(false)
	if editor.Text != input.Draft.Text || editor.CursorBack != input.Draft.CursorBack || len(editor.Skills) != 0 {
		t.Fatalf("Undo did not restore the original unbound draft: %+v", editor.Draft)
	}
	editor.Undo(true)
	conn.handleReceipt(sessionapi.NativeReceipt{Effect: effects[0].ID})
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Composer: &sessionapi.NativeComposerInput{
			Intent: sessionpresentation.ComposerIntent{Operation: sessionpresentation.SubmitComposer},
			Draft:  editor.Draft,
		},
	}); err != nil {
		t.Fatal(err)
	}
	srv.mu.Lock()
	prompts := slices.Clone(srv.params["turn/start"])
	srv.mu.Unlock()
	var sent struct {
		Input []struct {
			Type string `json:"type"`
			Text string `json:"text"`
			Name string `json:"name"`
			Path string `json:"path"`
		} `json:"input"`
	}
	if len(prompts) != 1 || json.Unmarshal(prompts[0], &sent) != nil || len(sent.Input) != 2 ||
		sent.Input[0].Text != "use $review-pr today" ||
		sent.Input[1].Type != "skill" || sent.Input[1].Name != "review-pr" ||
		sent.Input[1].Path != "/tmp/ws/.agents/skills/review-pr/SKILL.md" {
		t.Fatalf("expected completed text and an exact native skill binding, got %s", prompts)
	}

	// @ file completion queries the active thread's native workspace catalog.
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Completion: &sessionapi.NativeEditorInput{
			Target: conversation.EditorTarget{Revision: 2},
			Draft:  composer.Draft{Text: "check @main"},
		},
	})
	if err != nil {
		t.Fatalf("@ completion: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Menu == nil || len(ev.Frame.View.Menu.Choices) != 1 || ev.Frame.View.Menu.Choices[0].Name != "@cmd/main.go" {
		t.Fatalf("expected @cmd/main.go file choice, got %+v", ev.Frame.View.Menu)
	}

	// Bare /model opens MenuSettings populated from model/list (filtering hidden models).
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Command: new("/model"),
	})
	if err != nil {
		t.Fatalf("/model menu: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Menu == nil || ev.Frame.View.Menu.Kind != sessionpresentation.MenuSettings || len(ev.Frame.View.Menu.Choices) != 2 || ev.Frame.View.Menu.Choices[0].Name != "gpt-6.2-sol" || ev.Frame.View.Menu.Choices[1].Name != "gpt-5-standard" {
		t.Fatalf("expected /model settings menu with 2 visible models, got %+v", ev.Frame.View.Menu)
	}
	modelMenuID := ev.Frame.View.Menu.ID
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Menu: &sessionpresentation.MenuAction{
			Thread: "thr-main",
			Menu:   modelMenuID,
			Kind:   sessionpresentation.MenuAccept,
			Choice: completion.ChoiceID{Name: "gpt-6.2-sol"},
		},
	})
	if err != nil {
		t.Fatalf("accept /model menu: %v", err)
	}
	// Server confirms the selected model via thread/settings/updated so /tier inspects gpt-6.2-sol's catalog entry.
	conn.handleNotification(appserver.Message{
		Method: "thread/settings/updated",
		Params: jsontext.Value("{\"threadId\":\"thr-main\",\"threadSettings\":{\"model\":\"gpt-6.2-sol\",\"effort\":\"medium\",\"serviceTier\":null}}"),
	})
	_ = drain()

	// Bare /tier includes Default even when the catalog only advertises priority.
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Command: new("/tier"),
	})
	if err != nil {
		t.Fatalf("/tier menu: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Menu == nil || ev.Frame.View.Menu.Kind != sessionpresentation.MenuSettings || len(ev.Frame.View.Menu.Choices) != 2 || ev.Frame.View.Menu.Choices[0].Display != "Default" || ev.Frame.View.Menu.Choices[1].Display != "Fast" {
		t.Fatalf("expected /tier settings menu with default + Fast choices, got %+v", ev.Frame.View.Menu)
	}
	tierMenuID := ev.Frame.View.Menu.ID
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Menu: &sessionpresentation.MenuAction{
			Thread: "thr-main",
			Menu:   tierMenuID,
			Kind:   sessionpresentation.MenuAccept,
			Choice: completion.ChoiceID{Name: "default"},
		},
	})
	if err != nil {
		t.Fatalf("accept /tier menu: %v", err)
	}

	// /effort high sends "effort" (not "reasoningEffort").
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Command: new("/effort high"),
	})
	if err != nil {
		t.Fatalf("/effort high: %v", err)
	}

	srv.mu.Lock()
	updates := slices.Clone(srv.params["thread/settings/update"])
	srv.mu.Unlock()
	if len(updates) < 3 {
		t.Fatalf("expected 3 thread/settings/update calls, got %d", len(updates))
	}
	if !strings.Contains(string(updates[1]), "\"serviceTier\":null") {
		t.Fatalf("expected serviceTier:null for standard tier, got %s", string(updates[1]))
	}
	if !strings.Contains(string(updates[2]), "\"effort\":\"high\"") {
		t.Fatalf("expected effort:high, got %s", string(updates[2]))
	}

	// Nested thread/settings/updated notification updates model, effort, and clears tier on null.
	conn.handleNotification(appserver.Message{
		Method: "thread/settings/updated",
		Params: jsontext.Value("{\"threadId\":\"thr-main\",\"threadSettings\":{\"model\":\"gpt-6.2-sol\",\"effort\":\"high\",\"serviceTier\":null}}"),
	})
	conn.mu.Lock()
	gotModel, gotEffort, gotTier := conn.model, conn.effort, conn.tier
	conn.mu.Unlock()
	if gotModel != "gpt-6.2-sol" || gotEffort != "high" || gotTier != "" {
		t.Fatalf("unexpected settings after nested update: model=%q effort=%q tier=%q", gotModel, gotEffort, gotTier)
	}

	// skills/changed invalidates cached skills so the next $ completion refreshes from skills/list.
	conn.handleNotification(appserver.Message{Method: "skills/changed"})
	conn.mu.Lock()
	loadedAfterInvalidation := conn.skillsLoaded
	conn.mu.Unlock()
	if loadedAfterInvalidation {
		t.Fatalf("expected skills/changed to invalidate cached skills")
	}

	// model/list passes threadId so custom provider catalogs are queried.
	srv.mu.Lock()
	modelListParams := slices.Clone(srv.params["model/list"])
	srv.mu.Unlock()
	if len(modelListParams) == 0 || !strings.Contains(string(modelListParams[0]), "\"threadId\":\"thr-main\"") {
		t.Fatalf("expected model/list to include threadId, got %v", modelListParams)
	}

	// Listed model with empty serviceTiers offers only Default.
	conn.handleNotification(appserver.Message{
		Method: "thread/settings/updated",
		Params: jsontext.Value(`{"threadId":"thr-main","threadSettings":{"model":"gpt-5-standard","effort":"medium","serviceTier":null}}`),
	})
	_ = drain()
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/tier")}); err != nil {
		t.Fatalf("/tier for listed model without serviceTiers: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Menu == nil || len(ev.Frame.View.Menu.Choices) != 1 || ev.Frame.View.Menu.Choices[0].Name != "default" {
		t.Fatalf("expected only Default choice for listed model with empty serviceTiers, got %+v", ev.Frame.View.Menu)
	}

	// An unknown model must not invent advertised tiers.
	conn.handleNotification(appserver.Message{
		Method: "thread/settings/updated",
		Params: jsontext.Value(`{"threadId":"thr-main","threadSettings":{"model":"custom-unlisted-model","effort":"medium","serviceTier":null}}`),
	})
	_ = drain()
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/tier")}); err != nil {
		t.Fatalf("/tier for unlisted model: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Menu == nil || len(ev.Frame.View.Menu.Choices) != 1 || ev.Frame.View.Menu.Choices[0].Name != "default" {
		t.Fatalf("expected only Default choice for an unlisted model, got %+v", ev.Frame.View.Menu)
	}
}

func TestDirectNativeUsageRateLimitsAndQueueRefresh(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "native.sock")
	srv := startMockNativeAppServer(t, socket)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatalf("openDirectNativeConnection: %v", err)
	}
	defer conn.Close()
	<-srv.ready

	drain := func() sessionapi.NativeEvent {
		var last sessionapi.NativeEvent
		for {
			select {
			case ev := <-conn.events:
				last = ev
			default:
				return last
			}
		}
	}

	// Initial bootstrap calls account/rateLimits/read and populates Caption.Limits.
	ev := drain()
	if len(ev.Frame.View.Caption.Limits) != 2 || ev.Frame.View.Caption.Limits[0].Label != "5h" || ev.Frame.View.Caption.Limits[0].Used != 12 || ev.Frame.View.Caption.Limits[1].Label != "weekly" || ev.Frame.View.Caption.Limits[1].Used != 21 {
		t.Fatalf("expected 5h (12%%) and weekly (21%%) caption limits, got %+v", ev.Frame.View.Caption.Limits)
	}

	// Push tokenUsage updates for both main and child worker threads.
	conn.handleNotification(appserver.Message{
		Method: "thread/tokenUsage/updated",
		Params: jsontext.Value(`{
			"threadId":"thr-main",
			"tokenUsage":{
				"total":{"totalTokens":15000,"inputTokens":12000,"outputTokens":3000,"cachedInputTokens":2000,"cacheWriteInputTokens":500,"reasoningOutputTokens":400},
				"last":{"totalTokens":100000},
				"modelContextWindow":400000,
				"estimatedCostUsd":0.12,
				"costPartial":false,
				"outputTokensPerSecond":96.0,
				"workDurationMs":1500,
				"roundtrips":2
			}
		}`),
	})
	conn.handleNotification(appserver.Message{
		Method: "thread/tokenUsage/updated",
		Params: jsontext.Value(`{
			"threadId":"thr-worker",
			"tokenUsage":{
				"total":{"totalTokens":8000,"inputTokens":6000,"outputTokens":2000,"cachedInputTokens":0,"reasoningOutputTokens":0},
				"last":{"totalTokens":20000},
				"modelContextWindow":200000,
				"estimatedCostUsd":0.36,
				"costPartial":true,
				"outputTokensPerSecond":64.5,
				"workDurationMs":900,
				"roundtrips":1
			}
		}`),
	})
	ev = drain()

	// Main caption carries tok/s item and context window.
	if ev.Frame.View.Caption.ContextTokens != 100000 || ev.Frame.View.Caption.ContextWindow != 400000 {
		t.Fatalf("unexpected caption context: %+v", ev.Frame.View.Caption)
	}
	if len(ev.Frame.View.Caption.Items) != 1 || ev.Frame.View.Caption.Items[0].Text != "96.0 tok/s" {
		t.Fatalf("expected 96.0 tok/s caption item, got %+v", ev.Frame.View.Caption.Items)
	}

	// Both main (/root) and child (/root/worker) rows in Main.Roster carry cost, tokens, and throughput.
	if ev.Frame.Main == nil || len(ev.Frame.Main.Roster) != 2 {
		t.Fatalf("expected 2 roster rows, got %+v", ev.Frame.Main)
	}
	rootAgent := ev.Frame.Main.Roster[0].Agent
	workerAgent := ev.Frame.Main.Roster[1].Agent
	if rootAgent.Name != "/root" || !rootAgent.CostKnown || rootAgent.Cost != 0.12 || rootAgent.InputTokens != 12000 || rootAgent.OutputTokens != 3000 {
		t.Fatalf("unexpected root roster agent: %+v", rootAgent)
	}
	if workerAgent.Name != "/root/worker" || !workerAgent.CostKnown || !workerAgent.CostPartial || workerAgent.Cost != 0.36 || len(workerAgent.StatusItems) != 1 || workerAgent.StatusItems[0].Text != "64.5 tok/s" {
		t.Fatalf("unexpected worker roster agent: %+v", workerAgent)
	}

	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/session")}); err != nil {
		t.Fatal(err)
	}
	overview := awaitDirectEvent(t, conn, func(event sessionapi.NativeEvent) bool {
		return event.Frame.View.Panel != nil && event.Frame.View.Panel.Session
	})
	fields := overview.Frame.View.Panel.Fields
	if !slices.ContainsFunc(fields, func(field sessionpresentation.StatusField) bool {
		return field.Group == "Usage" && field.Label == "Uncached input tokens" && field.Value == "16K"
	}) {
		t.Fatalf("expected 18K input less 2K cached input in Session, got %+v", fields)
	}

	// Sparse account/rateLimits/updated merges updated primary window while preserving secondary.
	conn.handleNotification(appserver.Message{
		Method: "account/rateLimits/updated",
		Params: jsontext.Value(`{"rateLimits":{"limitId":"codex","primary":{"usedPercent":45.0,"windowDurationMins":300}}}`),
	})
	ev = drain()
	if len(ev.Frame.View.Caption.Limits) != 2 || ev.Frame.View.Caption.Limits[0].Used != 45 || ev.Frame.View.Caption.Limits[1].Used != 21 {
		t.Fatalf("expected merged caption limits [45%%, 21%%], got %+v", ev.Frame.View.Caption.Limits)
	}

	// thread/queue/changed (which sends only threadId) triggers thread/queue/list and decodes data[].input.
	srv.mu.Lock()
	srv.queueData = []map[string]any{
		{
			"id": "q-1",
			"input": []map[string]any{
				{"type": "text", "text": "run tests next"},
			},
			"clientUserMessageId": "msg-q-1",
		},
	}
	srv.mu.Unlock()
	srv.send(t, map[string]any{
		"jsonrpc": "2.0",
		"method":  "thread/queue/changed",
		"params":  map[string]any{"threadId": "thr-main"},
	})
	deadline := time.After(2 * time.Second)
	for {
		select {
		case next := <-conn.events:
			if len(next.Frame.View.Pending.Queued) == 1 && next.Frame.View.Pending.Queued[0].Text == "run tests next" {
				return
			}
		case <-deadline:
			t.Fatal("timed out waiting for thread/queue/changed to refresh queued submissions from thread/queue/list")
		}
	}
}

func TestDirectNativeSteersWaitingMessagesBeforeDeleting(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "native.sock")
	server := startMockNativeAppServer(t, socket)
	input := []map[string]any{
		{"type": "text", "text": "[Image map.png]"},
		{"type": "image", "url": "data:image/png;base64,AA=="},
	}
	metadata := map[string]string{"client_type": "terminal"}
	server.mu.Lock()
	server.queueData = []map[string]any{
		{"id": "first", "kind": "message", "input": input, "clientUserMessageId": "original-id", "responsesapiClientMetadata": metadata},
		{"id": "shell", "kind": "shellCommand", "input": []map[string]any{{"type": "text", "text": "!pwd"}}},
		{"id": "second", "kind": "message", "input": input, "clientUserMessageId": "second-id"},
	}
	server.overrides["turn/steer"] = func(raw jsontext.Value) any {
		if strings.Contains(string(raw), `"clientUserMessageId":"second-id"`) {
			return mockRPCError("expected active turn id changed")
		}
		return map[string]any{"turnId": "turn-active"}
	}
	server.mu.Unlock()
	ctx := t.Context()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	server.send(t, map[string]any{
		"jsonrpc": "2.0", "method": "turn/started",
		"params": map[string]any{"threadId": "thr-main", "turn": map[string]any{"id": "turn-active"}},
	})
	awaitDirectEvent(t, conn, func(event sessionapi.NativeEvent) bool {
		return event.Frame.View.Pending.CanSteer && len(event.Frame.View.Pending.Queued) == 3
	})
	_, err = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Composer: &sessionapi.NativeComposerInput{
			Intent: sessionpresentation.ComposerIntent{Thread: "thr-main", Operation: sessionpresentation.SteerWaiting},
		},
	})
	if err == nil || !strings.Contains(err.Error(), "expected active turn id changed") {
		t.Fatalf("stale steer result: %v", err)
	}
	server.mu.Lock()
	defer server.mu.Unlock()
	var methods []string
	for _, method := range server.methods {
		if method == "turn/steer" || method == "thread/queue/delete" {
			methods = append(methods, method)
		}
	}
	if want := []string{"turn/steer", "thread/queue/delete", "turn/steer"}; !slices.Equal(methods, want) {
		t.Fatalf("stock steer/delete order: %v", methods)
	}
	if got := server.params["thread/queue/delete"]; len(got) != 1 || !strings.Contains(string(got[0]), `"queuedSubmissionId":"first"`) {
		t.Fatalf("deleted a submission that was not admitted: %s", got)
	}
	var got map[string]jsontext.Value
	if err := json.Unmarshal(server.params["turn/steer"][0], &got); err != nil {
		t.Fatal(err)
	}
	wantInput, _ := json.Marshal(input, json.Deterministic(true))
	gotInput := got["input"]
	if err := gotInput.Canonicalize(); err != nil {
		t.Fatal(err)
	}
	wantMetadata, _ := json.Marshal(metadata)
	if string(gotInput) != string(wantInput) ||
		string(got["responsesapiClientMetadata"]) != string(wantMetadata) ||
		string(got["expectedTurnId"]) != `"turn-active"` {
		t.Fatalf("steer lost listed payload or active turn: %s", got)
	}
}

func TestDirectNativeEditWaitingStackKeepsImagesAndSkipsShell(t *testing.T) {
	var pngData bytes.Buffer
	if err := png.Encode(&pngData, image.NewRGBA(image.Rect(0, 0, 2, 3))); err != nil {
		t.Fatal(err)
	}
	encoded := "data:image/png;base64," + base64.StdEncoding.EncodeToString(pngData.Bytes())
	socket := filepath.Join(shortTempDir(t), "native.sock")
	srv := startMockNativeAppServer(t, socket)
	first := map[string]any{"id": "first", "kind": "message", "input": []map[string]any{
		{"type": "text", "text": "one "}, {"type": "image", "url": encoded},
	}}
	second := map[string]any{"id": "second", "kind": "message", "input": []map[string]any{
		{"type": "image", "url": encoded}, {"type": "text", "text": " two"},
	}}
	shell := map[string]any{"id": "shell", "kind": "shellCommand", "input": []map[string]any{
		{"type": "text", "text": "!pwd"},
	}}
	srv.mu.Lock()
	srv.queueData = []map[string]any{first, shell, second}
	srv.overrides["thread/queue/take"] = func(jsontext.Value) any {
		return map[string]any{"data": []any{first, second}}
	}
	srv.mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	var visible sessionapi.NativeEvent
	for len(visible.Frame.View.Pending.Queued) < 3 {
		select {
		case visible = <-conn.events:
		case <-ctx.Done():
			t.Fatal(ctx.Err())
		}
	}
	var key terminalsession.Main
	key.Update(visible.Frame.View)
	intent := key.ComposerKey("\x1b[1;3A", nil, nil).Command
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Composer: &sessionapi.NativeComposerInput{Intent: intent},
	}); err != nil {
		t.Fatal(err)
	}
	var result struct {
		IDs []string `json:"queuedSubmissionIds"`
	}
	srv.mu.Lock()
	calls := slices.Clone(srv.params["thread/queue/take"])
	srv.mu.Unlock()
	if len(calls) != 1 || json.Unmarshal(calls[0], &result) != nil || !slices.Equal(result.IDs, []string{"first", "second"}) {
		t.Fatalf("waiting edit should request only ordinary messages: %s", calls)
	}
	for {
		select {
		case ev := <-conn.events:
			for _, effect := range ev.Frame.Effects {
				if effect.Restore == nil {
					continue
				}
				copy := copyRestoredEffect(ctx, effect)
				defer copy.cleanup()
				if copy.err != nil {
					t.Fatal(copy.err)
				}
				editor := &composer.Editor{}
				editor.Replace(composer.Draft{Text: "draft"})
				editor.Restore(*copy.effect.Restore)
				if editor.Text != "one [Image 1]\n[Image 2] two\ndraft" || len(editor.Images) != 2 {
					t.Fatalf("restored editor: %+v", editor.Draft)
				}
				for _, attachment := range editor.Images {
					got, err := os.ReadFile(attachment.Path)
					if err != nil || !bytes.Equal(got, pngData.Bytes()) {
						t.Fatalf("restored image bytes differ: %v", err)
					}
				}
				return
			}
		case <-ctx.Done():
			t.Fatal(ctx.Err())
		}
	}
}
