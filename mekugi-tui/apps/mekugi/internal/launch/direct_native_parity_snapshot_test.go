package launch

import (
	"context"
	"encoding/json/jsontext"
	json "encoding/json/v2"
	"io"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/composition"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/appserver"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffrender"
	activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/composer"
	"github.com/mekugi-harness/mekugi/mekugi-tui/testsupport/uisnapshot"
)

func TestUISnapshotNativeArchiveAndWaitingInput(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.send(sessionapi.NativeOperation{Command: new("/archive")})
	menu := f.awaitMenu(sessionpresentation.MenuArchive)
	f.assert("archive-confirm-idle", menu, 74, 12)
	f.send(sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.Frame.View.Menu.ID, Kind: sessionpresentation.MenuAccept,
		Choice: menu.Frame.View.Menu.Choices[0].Identity(),
	}})

	f.server.mu.Lock()
	f.server.queueData = []map[string]any{
		{"id": "q1", "kind": "message", "input": appserver.Input("Check the parser diagnostics.")},
		{"id": "q2", "kind": "shell", "input": appserver.Input("!go test ./parser")},
		{"id": "q3", "kind": "message", "input": appserver.Input("Keep existing input intact.")},
	}
	f.server.mu.Unlock()
	f.notify("turn/started", `{"threadId":"thr-main","turn":{"id":"turn-1","status":"inProgress"}}`)
	f.notify("thread/queue/changed", `{"threadId":"thr-main"}`)
	queued := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Facts.Busy && len(ev.Frame.View.Pending.Queued) == 3
	})
	f.assert("waiting-input-mixed", queued, 54, 18)

	f.send(sessionapi.NativeOperation{Command: new("/archive")})
	f.assert("archive-confirm-running", f.awaitMenu(sessionpresentation.MenuArchive), 74, 12)
}

func TestUISnapshotNativeResumeAndRewind(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.server.mu.Lock()
	f.server.overrides["thread/list"] = func(params jsontext.Value) any {
		if strings.Contains(string(params), `"archived":true`) {
			return jsontext.Value(`{"data":[
				{"id":"archived","name":"Investigate yesterday's failure","cwd":"/projects/parser","updatedAt":1699992901,"gitInfo":{"branch":"fix/parser"}}
			]}`)
		}
		if strings.Contains(string(params), `"cwd":"/tmp/ws"`) {
			return jsontext.Value(`{"data":[
				{"id":"local","name":"Improve rendered snapshot coverage","cwd":"/tmp/ws","updatedAt":1700000001,"gitInfo":{"branch":"fix/coverage"}}
			]}`)
		}
		return jsontext.Value(`{"data":[
			{"id":"local","name":"Improve rendered snapshot coverage","cwd":"/tmp/ws","updatedAt":1700000001,"gitInfo":{"branch":"fix/coverage"}},
			{"id":"remote","preview":"Investigate a long parser failure","cwd":"/projects/parser","updatedAt":1699992901,"gitInfo":{"branch":"fix/parser"}}
		]}`)
	}
	f.server.overrides["thread/revert/preview"] = func(jsontext.Value) any {
		return jsontext.Value(`{"changeIds":["parser-edit","parser-tests"]}`)
	}
	f.server.mu.Unlock()
	f.send(sessionapi.NativeOperation{Command: new("/resume")})
	resume := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Resume != nil })
	f.assert("resume-local-narrow", resume, 34, 12)

	f.send(sessionapi.NativeOperation{Resume: &sessionpresentation.ResumeAction{
		Picker: resume.Frame.View.Resume.ID, Kind: sessionpresentation.ResumeToggleWorkspace,
	}})
	resume = f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Resume != nil && ev.Frame.View.Resume.All
	})
	f.assert("resume-all-metadata", resume, 98, 12)
	f.send(sessionapi.NativeOperation{Resume: &sessionpresentation.ResumeAction{
		Picker: resume.Frame.View.Resume.ID, Kind: sessionpresentation.ResumeToggleArchived,
	}})
	resume = f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Resume != nil && ev.Frame.View.Resume.Archived
	})
	f.assert("resume-archived", resume, 98, 12)
	f.send(sessionapi.NativeOperation{Resume: &sessionpresentation.ResumeAction{
		Picker: resume.Frame.View.Resume.ID, Kind: sessionpresentation.ResumeDismiss,
	}})

	f.send(sessionapi.NativeOperation{Command: new("/rewind")})
	menu := f.awaitMenu(sessionpresentation.MenuRewind)
	f.assert("rewind-user-messages", menu, 70, 12)
	f.send(sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.Frame.View.Menu.ID, Kind: sessionpresentation.MenuAccept,
		Choice: menu.Frame.View.Menu.Choices[1].Identity(),
	}})
	f.assert("rewind-captured-edits", f.awaitMenu(sessionpresentation.MenuRewindEdits), 70, 12)
}

func TestDirectNativeResumeStockAdmissionAndScopes(t *testing.T) {
	f := newNativeParitySnapshot(t)
	const id = "6caf6016-5bc6-4f9d-a1bb-cd473ca588ef"
	const namedID = "0479967a-de89-41c6-83d4-c386c0085704"
	const archivedID = "af68718a-fd4d-4088-bca1-d81f8f994684"
	f.server.mu.Lock()
	f.server.overrides["thread/list"] = func(params jsontext.Value) any {
		query := string(params)
		secondPage := strings.Contains(query, `"cursor":"next"`)
		switch {
		case strings.Contains(query, `"searchTerm":"Rename"`):
			return jsontext.Value(`{"data":[{"id":"` + namedID + `","name":"Renamed saved chat"}]}`)
		case strings.Contains(query, `"searchTerm":"Duplicate"`):
			if secondPage {
				return jsontext.Value(`{"data":[{"id":"` + archivedID + `","name":"Duplicate"}]}`)
			}
			return jsontext.Value(`{"data":[{"id":"` + id + `","name":"Duplicate"}],"nextCursor":"next"}`)
		case strings.Contains(query, `"searchTerm":"Renamed saved chat"`):
			if secondPage {
				return jsontext.Value(`{"data":[{"id":"` + namedID + `","name":"Renamed saved chat"}]}`)
			}
			return jsontext.Value(`{"data":[],"nextCursor":"next"}`)
		case strings.Contains(query, `"archived":true`):
			return jsontext.Value(`{"data":[{"id":"` + archivedID + `","name":"Archived chat","cwd":"/tmp/ws"}]}`)
		case secondPage:
			return jsontext.Value(`{"data":[{"id":"` + namedID + `","name":"Renamed saved chat","cwd":"/tmp/ws"}]}`)
		default:
			return jsontext.Value(`{"data":[],"nextCursor":"next"}`)
		}
	}
	f.server.overrides["thread/resume"] = func(params jsontext.Value) any {
		thread := id
		switch {
		case strings.Contains(string(params), namedID):
			thread = namedID
		case strings.Contains(string(params), archivedID):
			thread = archivedID
		}
		return jsontext.Value(`{"thread":{"id":"` + thread + `","cwd":"/tmp/ws"}}`)
	}
	f.server.mu.Unlock()

	submit := func(command string) error {
		_, err := f.conn.Send(f.ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
			Composer: &sessionapi.NativeComposerInput{Draft: composer.Draft{Text: command}},
		})
		return err
	}
	f.notify("turn/started", `{"threadId":"thr-main","turn":{"id":"turn-1","status":"inProgress"}}`)
	f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.Facts.Busy })
	for _, command := range []string{"/resume", "/resume " + id} {
		if err := submit(command); err == nil || !strings.Contains(err.Error(), "task is in progress") {
			t.Fatalf("%s while busy: %v", command, err)
		}
	}
	f.notify("turn/completed", `{"threadId":"thr-main","turn":{"id":"turn-1","status":"completed"}}`)
	f.await(func(ev sessionapi.NativeEvent) bool { return !ev.Frame.Facts.Busy })
	if err := submit("/resume Rename"); err == nil || !strings.Contains(err.Error(), "no saved chat") {
		t.Fatalf("partial label was not rejected: %v", err)
	}
	if err := submit("/resume Duplicate"); err == nil || !strings.Contains(err.Error(), "multiple sessions") {
		t.Fatalf("duplicate label was not rejected across pages: %v", err)
	}
	if err := submit("/resume " + id); err != nil {
		t.Fatal(err)
	}
	f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Main.ID == id })
	if err := submit("/resume " + strings.ReplaceAll(id, "-", "")); err != nil {
		t.Fatalf("reselect current thread by compact UUID: %v", err)
	}
	if err := submit("/resume Renamed saved chat"); err != nil {
		t.Fatal(err)
	}
	f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Main.ID == namedID })
	if err := submit("/resume"); err != nil {
		t.Fatal(err)
	}
	view := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Resume != nil }).Frame.View.Resume
	action := func(kind sessionpresentation.ResumeActionKind, thread, cursor string) {
		t.Helper()
		f.send(sessionapi.NativeOperation{Resume: &sessionpresentation.ResumeAction{
			Picker: view.ID, Kind: kind, Thread: thread, After: cursor,
		}})
	}
	action(sessionpresentation.ResumePage, "", view.PageAfter)
	f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Resume != nil && len(ev.Frame.View.Resume.Rows) == 1
	})
	action(sessionpresentation.ResumeToggleArchived, "", "")
	view = f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Resume != nil && ev.Frame.View.Resume.Archived
	}).Frame.View.Resume
	action(sessionpresentation.ResumeChoose, namedID, "")
	action(sessionpresentation.ResumeChoose, archivedID, "")
	f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Resume == nil && ev.Frame.View.Main.ID == archivedID
	})

	f.server.mu.Lock()
	defer f.server.mu.Unlock()
	lists := f.server.params["thread/list"]
	resumed := f.server.params["thread/resume"]
	restored := f.server.params["thread/unarchive"]
	if len(resumed) != 3 || !strings.Contains(string(resumed[0]), id) ||
		!strings.Contains(string(resumed[1]), namedID) || !strings.Contains(string(resumed[2]), archivedID) {
		t.Fatalf("wrong resume targets (or mutation on rejection): %s", resumed)
	}
	unarchive := slices.Index(f.server.methods, "thread/unarchive")
	if len(restored) != 1 || !strings.Contains(string(restored[0]), archivedID) ||
		unarchive < 0 || !slices.Contains(f.server.methods[unarchive+1:], "thread/resume") {
		t.Fatalf("expected exact unarchive before resume, got %s", restored)
	}
	if len(lists) != 8 {
		t.Fatalf("unexpected list requests: %s", lists)
	}
	for _, list := range lists {
		if !strings.Contains(string(list), `"modelProviders":[]`) {
			t.Fatalf("expected all providers, got %s", list)
		}
	}
	for _, list := range lists[:5] {
		if !strings.Contains(string(list), `"sortKey":"created_at"`) {
			t.Fatalf("named lookup must preserve creation order, got %s", list)
		}
	}
	for i, list := range lists[5:] {
		if !strings.Contains(string(list), `"cwd":"/tmp/ws"`) ||
			!strings.Contains(string(list), `"sortKey":"updated_at"`) ||
			strings.Contains(string(list), `"archived":true`) != (i == 2) {
			t.Fatalf("unexpected local/archived picker scope: %s", list)
		}
	}
}

func TestDirectNativeResumeLastUsesAllProvidersInWorkspace(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "native.sock")
	server := startMockNativeAppServer(t, socket)
	server.mu.Lock()
	server.overrides["thread/resume"] = func(jsontext.Value) any {
		return jsontext.Value(`{"thread":{"id":"thr-saved","cwd":"/tmp/ws"}}`)
	}
	server.mu.Unlock()
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	defer cancel()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "--last", "")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Main.ID == "thr-saved" })

	server.mu.Lock()
	defer server.mu.Unlock()
	list, resume := server.params["thread/list"], server.params["thread/resume"]
	var query struct {
		Limit              int      `json:"limit"`
		ModelProviders     []string `json:"modelProviders"`
		Archived           *bool    `json:"archived"`
		CurrentAccountOnly bool     `json:"currentAccountOnly"`
		SortKey            string   `json:"sortKey"`
		Cwd                string   `json:"cwd"`
	}
	if len(list) != 1 || json.Unmarshal(list[0], &query) != nil ||
		query.Limit != 1 || query.ModelProviders == nil || len(query.ModelProviders) != 0 ||
		query.Archived == nil || *query.Archived || !query.CurrentAccountOnly ||
		query.SortKey != "updated_at" || query.Cwd != "/tmp/ws" {
		t.Fatalf("unexpected --last lookup: %s", list)
	}
	if len(resume) != 1 || !strings.Contains(string(resume[0]), `"threadId":"thr-saved"`) ||
		len(server.params["thread/start"]) != 0 {
		t.Fatalf("unexpected startup methods %v, resume params %s", server.methods, resume)
	}
}

func TestDirectNativeResumeStartupDismissStartsFresh(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "native.sock")
	server := startMockNativeAppServer(t, socket)
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	defer cancel()
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "--pick", "")
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	event := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Resume != nil && ev.Frame.View.Resume.Startup
	})
	if _, err := conn.Send(ctx, event.Target(), sessionapi.NativeOperation{
		Resume: &sessionpresentation.ResumeAction{Kind: sessionpresentation.ResumeDismiss, Picker: event.Frame.View.Resume.ID},
	}); err != nil {
		t.Fatal(err)
	}
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Resume == nil && ev.Frame.View.Main.ID == "thr-main"
	})
	server.mu.Lock()
	defer server.mu.Unlock()
	if len(server.params["thread/start"]) != 1 || len(server.params["thread/resume"]) != 0 {
		t.Fatalf("startup dismissal did not start a fresh thread: %v", server.methods)
	}
}

func TestUISnapshotNativeFirstInputWhileTurnStarts(t *testing.T) {
	for _, reject := range []bool{false, true} {
		name := "accepted"
		if reject {
			name = "rejected"
		}
		t.Run(name, func(t *testing.T) {
			f := newNativeParitySnapshot(t)
			entered := make(chan string, 1)
			release := make(chan struct{})
			f.server.mu.Lock()
			f.server.overrides["turn/start"] = func(raw jsontext.Value) any {
				var p struct {
					ClientID string `json:"clientUserMessageId"`
				}
				_ = json.Unmarshal(raw, &p)
				entered <- p.ClientID
				select {
				case <-release:
				case <-f.ctx.Done():
					return mockRPCError("fixture canceled")
				}
				if reject {
					return mockRPCError("admission rejected")
				}
				return jsontext.Value(`{"turn":{"id":"first-turn","status":"inProgress"}}`)
			}
			f.server.mu.Unlock()

			before := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Draft.Available })
			if rows := f.rows(before, 76, 12); !strings.Contains(rows[0], "mekugi-app-server/test") {
				t.Fatalf("expected welcome before sending, got %q", rows[0])
			}
			const prompt = "Check the parser."
			submitted := make(chan error, 1)
			go func() {
				_, err := f.conn.Send(f.ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
					Composer: &sessionapi.NativeComposerInput{Draft: composer.Draft{Text: prompt}},
				})
				submitted <- err
			}()
			var clientID string
			select {
			case clientID = <-entered:
			case <-f.ctx.Done():
				t.Fatal("turn/start did not reach the mock server")
			}
			if clientID == "" {
				t.Fatal("turn/start did not supply a user message identity")
			}
			pending := f.await(func(ev sessionapi.NativeEvent) bool {
				return len(ev.Frame.Main.Entries) == 1 && ev.Frame.Main.Entries[0].Unconfirmed
			})
			seq := pending.Frame.Main.Entries[0].Seq
			if !reject {
				f.assert("first-input-before-ack", pending, 76, 12)
			}
			close(release)
			err := <-submitted
			if reject {
				if err == nil {
					t.Fatal("turn admission succeeded unexpectedly")
				}
				f.await(func(ev sessionapi.NativeEvent) bool { return len(ev.Frame.Main.Entries) == 0 })
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			f.await(func(ev sessionapi.NativeEvent) bool {
				return ev.Frame.View.Interrupt.Turn == "first-turn" && len(ev.Frame.Main.Entries) == 1
			})
			readStarted, releaseRead, readDone := make(chan struct{}), make(chan struct{}), make(chan struct{})
			f.server.mu.Lock()
			f.server.overrides["thread/read"] = func(jsontext.Value) any {
				close(readStarted)
				select {
				case <-releaseRead:
					return jsontext.Value(`{"thread":{"id":"thr-main","turns":[],
						"tokenUsage":{"estimatedCostUsd":0.12},"extra":{"skills":["review"]}}}`)
				case <-f.ctx.Done():
					return mockRPCError("fixture canceled")
				}
			}
			f.server.mu.Unlock()
			go func() {
				f.conn.reloadThreadTranscript(f.ctx, "thr-main")
				close(readDone)
			}()
			select {
			case <-readStarted:
			case <-f.ctx.Done():
				t.Fatal("stale thread read did not reach the mock server")
			}
			f.notify("item/completed", `{"threadId":"thr-main","turnId":"first-turn",
				"item":{"id":"pre-turn-compaction","type":"contextCompaction"}}`)
			f.notify("item/completed", `{"threadId":"thr-main","turnId":"first-turn",
				"item":{"id":"native-user","type":"userMessage","clientId":"`+clientID+`",
				"content":[{"type":"text","text":"`+prompt+`"}]}}`)
			accepted := f.await(func(ev sessionapi.NativeEvent) bool {
				return len(ev.Frame.Main.Entries) == 2 && !ev.Frame.Main.Entries[0].Unconfirmed
			})
			if entries := accepted.Frame.Main.Entries; entries[0].Seq != seq || entries[0].Text != prompt || entries[1].Kind != "compaction" {
				t.Fatalf("native user item did not replace the pending row in place: %+v", entries)
			}
			close(releaseRead)
			<-readDone
			refreshed := f.await(func(ev sessionapi.NativeEvent) bool {
				return ev.Frame.View.Caption.CostKnown && len(ev.Frame.View.Info.Skills) == 1
			})
			if entries := refreshed.Frame.Main.Entries; len(entries) != 2 || entries[0].Text != prompt {
				t.Fatalf("an older read displaced live first input: %+v", entries)
			}
		})
	}
}

func TestDirectNativeResumeTurnStartAcknowledgment(t *testing.T) {
	for _, tc := range []struct {
		name, thread, input string
		completesBeforeAck  bool
	}{
		{name: "Main reply before start", thread: "thr-main", input: "Start a task"},
		{name: "Main completion before reply", thread: "thr-main", input: "Start a task", completesBeforeAck: true},
		{name: "One-off reply before start", thread: "thr-side", input: "/oneoff Start a side task"},
		{name: "One-off completion before reply", thread: "thr-side", input: "/oneoff Start a side task", completesBeforeAck: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := newNativeParitySnapshot(t)
			const turn = "native-turn"
			entered, release := make(chan struct{}), make(chan struct{})
			f.server.mu.Lock()
			f.server.overrides["turn/start"] = func(jsontext.Value) any {
				// The reply and notification streams can be consumed in either
				// order. Control them independently instead of sleeping.
				close(entered)
				if tc.completesBeforeAck {
					select {
					case <-release:
					case <-f.ctx.Done():
						return mockRPCError("fixture canceled")
					}
				}
				return jsontext.Value(`{"turn":{"id":"` + turn + `","status":"inProgress"}}`)
			}
			f.server.mu.Unlock()
			submit := func(text string, operation sessionpresentation.ComposerOperation) error {
				_, err := f.conn.Send(f.ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
					Composer: &sessionapi.NativeComposerInput{
						Draft: composer.Draft{Text: text}, Intent: sessionpresentation.ComposerIntent{Operation: operation},
					},
				})
				return err
			}
			notify := func(method, status string) {
				f.notify(method, `{"threadId":"`+tc.thread+`","turn":{"id":"`+turn+`","status":"`+status+`"}}`)
			}
			awaitActive := func() {
				f.await(func(ev sessionapi.NativeEvent) bool {
					return ev.Frame.Facts.Busy && ev.Frame.View.Interrupt.Thread == tc.thread &&
						ev.Frame.View.Interrupt.Turn == turn
				})
			}
			awaitIdle := func() {
				f.await(func(ev sessionapi.NativeEvent) bool {
					return !ev.Frame.Facts.Busy && ev.Frame.View.Input.ID == tc.thread
				})
			}
			if tc.completesBeforeAck {
				result := make(chan error, 1)
				go func() { result <- submit(tc.input, sessionpresentation.SubmitComposer) }()
				select {
				case <-entered:
				case <-f.ctx.Done():
					t.Fatal("turn/start did not reach the mock server")
				}
				notify("turn/started", "inProgress")
				awaitActive()
				notify("turn/completed", "interrupted")
				awaitIdle()
				close(release)
				if err := <-result; err != nil {
					t.Fatal(err)
				}
			} else {
				if err := submit(tc.input, sessionpresentation.SubmitComposer); err != nil {
					t.Fatal(err)
				}
				if err := submit("/resume", sessionpresentation.SubmitComposer); err == nil ||
					!strings.Contains(err.Error(), "task is in progress") {
					t.Fatalf("start acknowledged but no turn/started yet: /resume err=%v", err)
				}
				awaitActive()
				if err := submit("Steer the acknowledged turn", sessionpresentation.SubmitComposer); err != nil {
					t.Fatal(err)
				}
				if err := submit("Queue after the acknowledged turn", sessionpresentation.QueueComposer); err != nil {
					t.Fatal(err)
				}
				notify("turn/started", "inProgress")
				notify("turn/completed", "completed")
				awaitIdle()
			}
			if err := submit("/resume", sessionpresentation.SubmitComposer); err != nil {
				t.Fatalf("settled turn incorrectly blocked /resume: %v", err)
			}
			f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Resume != nil })
			f.server.mu.Lock()
			defer f.server.mu.Unlock()
			if len(f.server.params["thread/list"]) != 1 {
				t.Fatal("the busy /resume reached the list endpoint")
			}
			if !tc.completesBeforeAck {
				steer, queue := f.server.params["turn/steer"], f.server.params["thread/queue/add"]
				if len(steer) != 1 || !strings.Contains(string(steer[0]), `"expectedTurnId":"`+turn+`"`) ||
					len(queue) != 1 || !strings.Contains(string(queue[0]), `"threadId":"`+tc.thread+`"`) {
					t.Fatalf("steer or queue lost the acknowledged turn: steer=%s, queue=%s", steer, queue)
				}
			}
		})
	}
}

func TestUISnapshotNativeApprovalDock(t *testing.T) {
	for _, tc := range []struct {
		name, method, params string
		width                int
	}{
		{"command", "item/commandExecution/requestApproval", `{
			"threadId":"thr-main","turnId":"turn-1","itemId":"cmd-1",
			"command":"git tag preview","cwd":"/tmp/ws","reason":"Tag the reviewed build."
		}`, 34},
		{"guard", "shellGuard/requestApproval", `{
			"threadId":"thr-main","turnId":"turn-1","itemId":"cmd-1",
			"executable":"/usr/bin/git","argv":["git","tag","preview"],"cwd":"/tmp/ws","kind":"write"
		}`, 74},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := newNativeParitySnapshot(t)
			f.server.send(t, map[string]any{
				"jsonrpc": "2.0", "id": 108, "method": tc.method, "params": jsontext.Value(tc.params),
			})
			ev := f.await(func(ev sessionapi.NativeEvent) bool { return len(ev.Frame.Approvals) == 1 })
			rows := append(f.rows(ev, tc.width, 24), "plain after dock")
			uisnapshot.AssertTerminal(t, "testdata/snapshots/native-parity-approval-"+tc.name+".txt", rows, tc.width)
		})
	}
}

func TestUISnapshotNativeQuestionDock(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("item/completed", `{
		"threadId":"thr-main","turnId":"turn-1","item":{"id":"question","type":"agentMessage",
			"text":"Which fixtures should I update?\n- Everything\n- Changed parser",
			"phase":"final_answer","delivery":"async","questions":[{
				"id":"question-1","header":"Scope","question":"Which fixtures should I update?",
				"options":[
					{"label":"Everything","description":"Update all parser fixtures."},
					{"label":"Changed parser","description":"Update only the affected fixtures.",
						"preview":"**Keep unchanged fixtures.**","recommended":true}
				]
			}]
		}
	}`)
	ev := f.await(func(ev sessionapi.NativeEvent) bool { return len(ev.Frame.Questions) > 0 })
	const width = 74
	rows := append(f.rows(ev, width, 26), "plain after dock")
	uisnapshot.AssertTerminal(t, "testdata/snapshots/native-parity-question-dock.txt", rows, width)
}

func TestUISnapshotNativeFileAndPresetCompletion(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.server.mu.Lock()
	f.server.overrides["thread/files/search"] = func(jsontext.Value) any {
		return jsontext.Value(`{"paths":["internal/uisnapshot/snapshot.go","internal/terminalview/snapshot_test.go"]}`)
	}
	f.server.overrides["thread/presets/list"] = func(jsontext.Value) any {
		return jsontext.Value(`{"presets":{"1":{"input":"Review the touched files.","label":"review-pr"}}}`)
	}
	f.server.mu.Unlock()

	f.client.accept(f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Main.ID == "thr-main" }))
	complete := func(text string) sessionapi.NativeEvent {
		t.Helper()
		editor := f.client.editor("main")
		editor.Replace(composer.Draft{Text: text})
		_, target := f.client.selectedEditor()
		f.send(sessionapi.NativeOperation{Completion: &sessionapi.NativeEditorInput{Draft: editor.Draft, Target: target}})
		return f.await(func(ev sessionapi.NativeEvent) bool {
			return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == sessionpresentation.MenuCompletion &&
				ev.Frame.CompletionTarget == target
		})
	}
	f.assert("file-completion-narrow", complete("@snapshot"), 42, 12)
	f.assert("skill-and-preset-completion", complete("$rev"), 76, 12)
}

func TestUISnapshotNativeShellComposer(t *testing.T) {
	for _, tc := range []struct {
		name  string
		theme diffrender.Theme
	}{
		{"terminal", diffrender.TerminalTheme},
		{"dark", diffrender.DarkTheme},
		{"light", diffrender.LightTheme},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := newNativeParitySnapshot(t)
			terminal, _ := f.client.renderer.views.NewAttachment(tc.theme)
			f.client.renderer.terminal = terminal
			ev := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.View.Main.ID == "thr-main" })
			const width = 44
			const script = "printf '%s\\n' \"你好\n$HOME\" | cat\n# comment"
			f.client.editor("main").Replace(composer.Draft{Text: "!" + script})
			rows := append(f.rows(ev, width, 10), "plain after shell")
			f.client.editor("main").Replace(composer.Draft{Text: script})
			rows = append(rows, f.rows(ev, width, 10)...)
			uisnapshot.AssertTerminal(t, "testdata/snapshots/native-parity-shell-"+tc.name+".txt", rows, width)
		})
	}
}

func TestUISnapshotNativeBTWAlongsideOneoff(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("thread/tokenUsage/updated", `{
		"threadId":"thr-main","tokenUsage":{"total":{"inputTokens":9000,"outputTokens":8000},"estimatedCostUsd":9.99}
	}`)
	f.send(sessionapi.NativeOperation{Command: new("/oneoff Check the parser.")})
	f.notify("turn/started", `{"threadId":"thr-side","turn":{"id":"side-turn","status":"inProgress"}}`)
	f.send(sessionapi.NativeOperation{Command: new("/btw Why did the tests fail?")})
	f.notify("item/completed", `{
		"threadId":"thr-side","turnId":"side-turn","item":{"id":"side-prompt","type":"userMessage","content":[
			{"type":"text","text":"Check "},{"type":"localImage","path":"fixture.png"},{"type":"text","text":" for clues."}
		]}
	}`)
	f.notify("thread/tokenUsage/updated", `{
		"threadId":"thr-side","tokenUsage":{"total":{"inputTokens":1200,"outputTokens":36},"estimatedCostUsd":0.12}
	}`)
	f.notify("item/agentMessage/delta", `{
		"threadId":"thr-side","turnId":"side-turn","itemId":"side-answer","delta":"Reviewing the parser."
	}`)
	f.notify("item/agentMessage/delta", `{
		"threadId":"thr-btw","turnId":"btw-turn","itemId":"btw-answer","delta":"The fixture used the wrong input."
	}`)
	ev := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Displayed.ID == "thr-side" && ev.Frame.View.BTW != nil && len(ev.Frame.View.BTW.Answer) > 0
	})
	const width = 92
	uisnapshot.AssertTerminal(t, "testdata/snapshots/native-parity-btw-alongside-oneoff.txt", f.rows(ev, width, 20), width)
}

func TestUISnapshotNativeProcessFamily(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.server.mu.Lock()
	f.server.bgTerminals = []map[string]any{
		{"threadId": "thr-main", "turnId": "main-turn", "itemId": "main-cmd",
			"processId": "proc-local", "command": "tail -f main.log", "cwd": "/tmp/ws"},
		{"threadId": "thr-worker", "turnId": "worker-turn", "itemId": "worker-cmd",
			"processId": "proc-worker", "command": "tail -f worker.log", "cwd": "/tmp/ws"},
	}
	f.server.mu.Unlock()
	f.send(sessionapi.NativeOperation{Command: new("/ps")})
	active := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Details != nil && ev.Frame.Details.View.Processes && len(ev.Frame.Processes) == 2
	})
	f.assert("process-family-active", active, 76, 18)

	f.notify("item/completed", `{
		"threadId":"thr-worker","turnId":"worker-turn","item":{"type":"commandExecution",
			"id":"worker-cmd","command":"tail -f worker.log","exitCode":2,"processId":"proc-worker",
			"status":"failed","aggregatedOutput":"worker.log is unavailable\n"}
	}`)
	settled := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Details != nil && slices.ContainsFunc(ev.Frame.Details.View.Pages, func(p sessionpresentation.DetailItem) bool {
			return p.Exited && p.ExitCode == 2
		})
	})
	f.client.renderer.terminal.Dialog.ShowPage(1)
	f.assert("process-family-retained", settled, 76, 18)
}

func TestUISnapshotNativeWorkingFooter(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.server.mu.Lock()
	f.server.overrides["thread/subtree/list"] = func(jsontext.Value) any {
		return jsontext.Value(`{"data":[
			{"id":"thr-main","status":{"type":"active"}},
			{"id":"thr-worker","parentThreadId":"thr-main","source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thr-main","depth":1,"agent_path":"/root/worker"}}},"status":{"type":"idle"},"agentStatus":{"completed":null},
			 "tokenUsage":{"total":{"inputTokens":300,"outputTokens":6},"estimatedCostUsd":0.18}},
			{"id":"thr-interrupted","parentThreadId":"thr-main","source":{"subAgent":{"thread_spawn":{"parent_thread_id":"thr-main","depth":1,"agent_path":"/root/interrupted"}}},"status":{"type":"idle"},"agentStatus":"interrupted"}
		]}`)
	}
	f.server.bgTerminals = []map[string]any{
		{"threadId": "thr-main", "turnId": "turn-1", "itemId": "one", "processId": "one"},
		{"threadId": "thr-main", "turnId": "turn-1", "itemId": "two", "processId": "two"},
	}
	f.server.mu.Unlock()
	f.conn.refreshSubtree(f.ctx, "thr-main")
	if err := f.conn.refreshProcesses(f.ctx, "thr-main"); err != nil {
		t.Fatal(err)
	}
	f.notify("thread/name/updated", `{"threadId":"thr-main","threadName":"Check the parser"}`)
	f.notify("thread/skills/updated", `{"threadId":"thr-main","names":["review"]}`)
	f.notify("turn/started", `{"threadId":"thr-main","turn":{"id":"turn-1","startedAt":1699999990}}`)
	f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-1",
		"item":{"type":"userMessage","id":"parser-prompt","content":[{"type":"text","text":"Check the parser."}]}}`)
	f.notify("thread/tokenUsage/updated", `{
		"threadId":"thr-main","tokenUsage":{"total":{"inputTokens":1200,"outputTokens":36},"estimatedCostUsd":0.12}
	}`)
	ev := f.await(func(ev sessionapi.NativeEvent) bool {
		v := ev.Frame.View
		return v.Status.Processes == 2 && v.Status.Session.Running && v.Caption.CostKnown &&
			len(v.Info.Skills) == 1 && v.Info.Title == "Check the parser"
	})
	f.assert("working-footer", ev, 112, 16)
}

func TestUISnapshotNativeReasoningLive(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("item/started", `{"threadId":"thr-main","turnId":"turn-1","item":{"type":"reasoning","id":"thinking"}}`)
	empty := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 1
	})
	f.assert("reasoning-empty", empty, 76, 12)
	f.notify("item/reasoning/summaryTextDelta", `{
		"threadId":"thr-main","turnId":"turn-1","itemId":"thinking","summaryIndex":0,
		"delta":"Inspecting the parser\n\nCompare valid and invalid inputs."
	}`)
	f.notify("item/reasoning/summaryTextDelta", `{
		"threadId":"thr-main","turnId":"turn-1","itemId":"thinking","summaryIndex":1,
		"delta":"**Checking results**\n\nStream the second section."
	}`)
	f.notify("item/started", `{
		"threadId":"thr-main","turnId":"turn-1","item":{"id":"batch","type":"commandExecution",
			"command":"date; pwd","status":"inProgress",
			"display":{"script":"date; pwd","segments":["date","pwd"],"stepKinds":["run","run"]}}
	}`)
	live := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 2
	})
	f.assert("reasoning-and-commands-live", live, 76, 16)
}

func TestUISnapshotNativeReasoningAndCommands(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("item/started", `{"threadId":"thr-main","turnId":"turn-1","item":{"type":"reasoning","id":"thinking"}}`)
	f.notify("item/reasoning/summaryTextDelta", `{
		"threadId":"thr-main","turnId":"turn-1","itemId":"thinking","summaryIndex":0,
		"delta":"Inspecting the parser\n\nCompare valid and invalid inputs."
	}`)
	f.notify("item/reasoning/summaryTextDelta", `{
		"threadId":"thr-main","turnId":"turn-1","itemId":"thinking","summaryIndex":1,
		"delta":"**Checking results**\n\nStream the second section."
	}`)
	f.notify("item/completed", `{
		"threadId":"thr-main","turnId":"turn-1","item":{"id":"thinking","type":"reasoning",
			"summary":["Inspecting the parser\n\nCompare valid and invalid inputs.","**Checking results**\n\nStream the second section."],"content":[]}
	}`)
	for _, item := range []string{
		`{"id":"batch","type":"commandExecution","command":"date; pwd","status":"completed",
			"display":{"script":"date; pwd","segments":["date","pwd"],"stepKinds":["run","run"]}}`,
		`{"id":"read","type":"commandExecution","command":"cat fixture.txt && rg invalid .","status":"completed","exitCode":0,
			"display":{"script":"cat fixture.txt && rg invalid .","segments":["cat fixture.txt","rg invalid ."],"stepKinds":["read","search"],"stepSubjects":["fixture.txt","invalid ."]},
			"aggregatedOutput":"one\nprogress 10%\rprogress 100%\n"}`,
		`{"id":"skill","type":"commandExecution","command":"skills-mgr get demo/references/check.md","status":"completed","exitCode":0,
			"display":{"script":"skills-mgr get demo/references/check.md","segments":[],"stepKinds":["skill"],"stepSubjects":["demo/references/check.md"]}}`,
		`{"id":"image-1","type":"imageView","path":"/tmp/ws/one.png"}`,
		`{"id":"image-2","type":"imageView","path":"/tmp/ws/two.png"}`,
		`{"id":"image-3","type":"imageView","path":"/tmp/ws/three.png"}`,
		`{"id":"image-4","type":"imageView","path":"/tmp/ws/four.png"}`,
		`{"id":"test","type":"commandExecution","command":"go test ./parser","status":"failed","exitCode":2}`,
		`{"id":"status","type":"commandExecution","command":"git status --short","status":"completed","exitCode":0}`,
		`{"id":"answer","type":"agentMessage","phase":"final_answer","text":"The fixture is invalid."}`,
	} {
		f.notify("item/completed", `{"threadId":"thr-main","turnId":"turn-1","item":`+item+`}`)
	}
	ev := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 11
	})
	const width = 76
	rows := append(f.rows(ev, width, 24), "plain after transcript")
	uisnapshot.AssertTerminal(t, "testdata/snapshots/native-parity-reasoning-and-commands.txt", rows, width)
}

func TestUISnapshotNativeFileChangeDetails(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.notify("item/completed", `{
		"threadId":"thr-main","turnId":"turn-1","item":{"type":"fileChange","id":"edits","status":"completed","changes":[
			{"path":"added.go","kind":{"type":"add"},"diff":"new\n"},
			{"path":"parser.go","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-before\n+after"},
			{"path":"old.go","kind":{"type":"delete"},"diff":"old\n"}
		]}
	}`)
	published := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 1 &&
			len(ev.Frame.Main.Entries[0].Values) == 3
	})
	f.send(sessionapi.NativeOperation{Details: &sessionapi.NativeDetailRequest{
		Kind: sessionapi.NativeEntryDetails, Source: published.Frame.Main.Entries[0].Seq,
	}})
	details := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.Details != nil })
	f.assert("file-change-details", details, 94, 17)
}

type nativeParitySnapshot struct {
	t      *testing.T
	ctx    context.Context
	server *mockNativeAppServer
	conn   *directNativeConnection
	client nativeClient
}

func newNativeParitySnapshot(t *testing.T) *nativeParitySnapshot {
	t.Helper()
	socket := filepath.Join(shortTempDir(t), "native.sock")
	server := startMockNativeAppServer(t, socket)
	server.mu.Lock()
	server.overrides["thread/changes/read"] = func(jsontext.Value) any {
		return jsontext.Value(`{"workspace":"/tmp/ws","changes":[],"files":[]}`)
	}
	server.mu.Unlock()
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	t.Cleanup(cancel)
	conn, err := openDirectNativeConnection(ctx, socket, "/tmp/ws", nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	views, err := composition.NativeRenderer(false)
	if err != nil {
		t.Fatal(err)
	}
	terminal, _ := views.NewAttachment(diffrender.DarkTheme)
	return &nativeParitySnapshot{t: t, ctx: ctx, server: server, conn: conn, client: nativeClient{
		connection: conn, renderer: nativeClientRenderer{views: views, terminal: terminal},
	}}
}

func (f *nativeParitySnapshot) send(op sessionapi.NativeOperation) {
	f.t.Helper()
	if _, err := f.conn.Send(f.ctx, sessionapi.NativeTarget{}, op); err != nil {
		f.t.Fatal(err)
	}
}

func (f *nativeParitySnapshot) notify(method, params string) {
	f.t.Helper()
	f.server.send(f.t, map[string]any{"jsonrpc": "2.0", "method": method, "params": jsontext.Value(params)})
}

func (f *nativeParitySnapshot) await(match func(sessionapi.NativeEvent) bool) sessionapi.NativeEvent {
	f.t.Helper()
	return awaitDirectEvent(f.t, f.conn, match)
}

func (f *nativeParitySnapshot) awaitMenu(kind sessionpresentation.MenuKind) sessionapi.NativeEvent {
	f.t.Helper()
	return f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Menu != nil && ev.Frame.View.Menu.Kind == kind
	})
}

func (f *nativeParitySnapshot) assert(name string, ev sessionapi.NativeEvent, width, height int) {
	f.t.Helper()
	uisnapshot.Assert(f.t, "testdata/snapshots/native-parity-"+name+".txt", strings.Join(f.rows(ev, width, height), "\n"))
}

func (f *nativeParitySnapshot) rows(ev sessionapi.NativeEvent, width, height int) []string {
	f.t.Helper()
	now := time.Date(2023, 11, 14, 22, 15, 1, 0, time.UTC)
	// Keep arrival times out of the golden without changing the projected
	// entries or the published completion state.
	stamp := time.Date(2023, 11, 14, 22, 15, 1, 0, time.Local)
	fixTimes := func(publication *activity.Publication) *activity.Publication {
		if publication == nil || len(publication.Entries) == 0 {
			return publication
		}
		fixed := *publication
		fixed.Entries = slices.Clone(publication.Entries)
		for i := range fixed.Entries {
			fixed.Entries[i].Observed = stamp
			if !fixed.Entries[i].Completed.IsZero() {
				fixed.Entries[i].Completed = stamp
			}
		}
		return &fixed
	}
	main := fixTimes(ev.Frame.Main)
	if ev.Frame.Agents == ev.Frame.Main {
		ev.Frame.Agents = main
	} else {
		ev.Frame.Agents = fixTimes(ev.Frame.Agents)
	}
	ev.Frame.Main = main
	if view := ev.Frame.View.Resume; view != nil {
		fixed := *view
		fixed.LoadedAt = now
		ev.Frame.View.Resume = &fixed
	}
	renderer := &f.client.renderer
	renderer.terminal.Width, renderer.terminal.Height = width, height
	f.client.accept(ev)
	f.client.refresh(now)
	rendered, err := renderer.render(f.ctx, io.Discard, now, "0.172.0")
	if err != nil {
		f.t.Fatal(err)
	}
	if len(rendered.Demands) != 0 {
		f.t.Fatalf("snapshot left unresolved output demands: %+v", rendered.Demands)
	}
	return rendered.Rows
}
