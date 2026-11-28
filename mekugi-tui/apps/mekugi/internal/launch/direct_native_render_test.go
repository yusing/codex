package launch

import (
	"context"
	"encoding/json/jsontext"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/ansi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/composition"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/terminalsession"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/appserver"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffrender"
	activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/composer"
	conversation "github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/testsupport/uisnapshot"
)

func TestDirectNativeProjectsIntoTerminalSessionFrame(t *testing.T) {
	ws := t.TempDir()
	sampleFile := filepath.Join(ws, "sample.go")
	if err := os.WriteFile(sampleFile, []byte("package demo\n\nfunc Hello() string { return \"hi\" }\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	socket := filepath.Join(shortTempDir(t), "native.sock")
	srv := startMockNativeAppServer(t, socket)
	srv.mu.Lock()
	srv.bgTerminals = []map[string]any{
		{
			"threadId":  "thr-main",
			"turnId":    "turn-1",
			"itemId":    "cmd-bg",
			"processId": "proc-42",
			"command":   "tail -f app.log",
			"cwd":       ws,
		},
		{
			"threadId":  "thr-worker",
			"turnId":    "turn-worker",
			"itemId":    "cmd-worker",
			"processId": "proc-42",
			"command":   "tail -f worker.log",
			"cwd":       ws,
		},
	}
	srv.mu.Unlock()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	conn, err := openDirectNativeConnection(ctx, socket, ws, nil, nil, "", "")
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

	// Both established names reach the reasoning picker; service tier has one name.
	hasEffort, hasReasoning, hasRewind, hasTier := false, false, false, false
	for _, ch := range directSlashCommands {
		switch ch.Name {
		case "/effort":
			hasEffort = true
		case "/reasoning":
			hasReasoning = true
		case "/rewind":
			hasRewind = true
		case "/tier":
			hasTier = true
		}
	}
	if !hasEffort || !hasReasoning || !hasRewind || !hasTier {
		t.Fatalf("unexpected slash catalog flags: effort=%v reasoning=%v rewind=%v tier=%v", hasEffort, hasReasoning, hasRewind, hasTier)
	}

	// Push real codex-rs wire shapes: commandExecution, fileChange, assistant reply, tokenUsage (main + worker).
	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{
			"threadId":"thr-main","turnId":"turn-1",
			"item":{
				"type":"commandExecution","id":"cmd-1",
				"command":"/bin/bash -lc 'echo hello-world'",
				"display":{"script":"echo hello-world","segments":[],"effectiveCwd":null},
				"status":"completed","exitCode":0,
				"aggregatedOutput":"line-1\nline-2\nline-3\nline-4\nline-5\nline-6\nline-7\nhello-world\n"
			}
		}`),
	})
	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{
			"threadId":"thr-main","turnId":"turn-1",
			"item":{
				"type":"fileChange","id":"fc-1","status":"completed",
				"changes":[{"path":"main.go","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-old\n+new\n"}]
			}
		}`),
	})
	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{
			"threadId":"thr-main","turnId":"turn-1",
			"item":{"type":"agentMessage","id":"msg-1","text":"Done fixing regressions."}
		}`),
	})
	conn.handleNotification(appserver.Message{
		Method: "thread/tokenUsage/updated",
		Params: jsontext.Value(`{
			"threadId":"thr-main",
			"tokenUsage":{
				"total":{"totalTokens":15000,"inputTokens":12000,"outputTokens":3000,"cachedInputTokens":1000,"reasoningOutputTokens":200},
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
				"total":{"totalTokens":5000,"inputTokens":4000,"outputTokens":1000,"cachedInputTokens":0,"reasoningOutputTokens":0},
				"last":{"totalTokens":10000},
				"modelContextWindow":200000,
				"estimatedCostUsd":0.36,
				"costPartial":false,
				"outputTokensPerSecond":64.0,
				"workDurationMs":800,
				"roundtrips":1
			}
		}`),
	})
	ev := drain()

	// Feed the projected NativeFrame through the real nativeClientRenderer + terminalsession.Frame.Render pipeline.
	views, err := composition.NativeRenderer(false)
	if err != nil {
		t.Fatalf("NativeRenderer: %v", err)
	}
	terminal, _ := views.NewAttachment(diffrender.DarkTheme)
	terminal.Width, terminal.Height = 160, 40
	renderer := nativeClientRenderer{terminal: terminal, views: views}
	now := time.Date(2026, 11, 26, 12, 0, 0, 0, time.Local)

	renderFrame := func(frame sessionapi.NativeFrame) (string, []activity.OutputDemand) {
		renderer.update(frame, conversation.DraftView{}, conversation.PromptDraftView{}, now)
		in := terminal.FrameInput(now, "0.172.0")
		if frame.Family != nil {
			in.FamilyRoot = frame.Family.Root
		}
		rendered, err := terminal.Frame.Render(ctx, in)
		if err != nil {
			t.Fatalf("Frame.Render: %v", err)
		}
		plain := make([]string, len(rendered.Rows))
		for i, r := range rendered.Rows {
			plain[i] = ansi.Strip(r)
		}
		return strings.Join(plain, "\n"), rendered.Demands
	}
	renderText := func(frame sessionapi.NativeFrame) string {
		s, demands := renderFrame(frame)
		if len(demands) > 0 {
			if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
				Output: &sessionapi.NativeOutputDemand{Demands: demands},
			}); err != nil {
				t.Fatalf("satisfy output demands: %v", err)
			}
			ev = drain()
			s, demands = renderFrame(ev.Frame)
			if len(demands) > 0 {
				t.Fatalf("unresolved output demands after hydration: %+v", demands)
			}
		}
		return s
	}

	screen := renderText(ev.Frame)
	for _, want := range []string{
		"echo hello-world",
		"main.go",
		"Done fixing regressions.",
		"$0.12",
		"total $0.48",
		"96.0 tok/s",
		"5h 88% left",
		"Weekly 79% left",
	} {
		if !strings.Contains(screen, want) {
			t.Fatalf("rendered TUI frame missing %q:\n%s", want, screen)
		}
	}
	if strings.Contains(screen, "/bin/bash -lc") {
		t.Fatalf("rendered TUI frame leaked raw shell wrapper:\n%s", screen)
	}

	// Default compact mode (Expansion == 0) clips long command output and collapses edit diffs;
	// two ctrl+e presses (Expansion == 2) expand full command output and edit diff once.
	if strings.Contains(screen, "line-1") {
		t.Fatalf("expected default compact mode to clip early output lines:\n%s", screen)
	}
	terminal.Transcript.ToggleExpansion()
	terminal.Transcript.ToggleExpansion()
	// Render once to collect the real OutputDemands emitted by terminalsession (just like nativeClient).
	_, demands := renderFrame(ev.Frame)
	if len(demands) == 0 {
		t.Fatalf("expected OutputDemands when expanding command output with ctrl+e")
	}
	_, _ = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Output: &sessionapi.NativeOutputDemand{Demands: demands},
	})
	ev = drain()
	expandedScreen := renderText(ev.Frame)
	if !strings.Contains(expandedScreen, "line-1") {
		t.Fatalf("expected ctrl+e expanded mode (Expansion == 2) to show full command output:\n%s", expandedScreen)
	}
	if !strings.Contains(expandedScreen, "Edited") || !strings.Contains(expandedScreen, "main.go") {
		t.Fatalf("expected Edited main.go in transcript:\n%s", expandedScreen)
	}
	terminal.Transcript.ToggleExpansion()

	// Open Details dialog on the commandExecution entry and hydrate DetailOutput.
	cmdSeq := conn.itemsByID[directItemKey{"thr-main", "cmd-1"}].seq
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Details: &sessionapi.NativeDetailRequest{
			Kind:   sessionapi.NativeEntryDetails,
			Source: cmdSeq,
		},
	}); err != nil {
		t.Fatalf("open entry details: %v", err)
	}
	ev = drain()
	if ev.Frame.Details == nil || len(ev.Frame.Details.View.Pages) == 0 {
		t.Fatalf("expected open Details dialog, got nil")
	}
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		DetailOutput: &sessionpresentation.DetailOutputRequest{
			ID:      ev.Frame.Details.View.ID,
			Page:    ev.Frame.Details.View.Pages[0].ID,
			Version: ev.Frame.Details.View.Pages[0].OutputVersion,
		},
	}); err != nil {
		t.Fatalf("detail output: %v", err)
	}
	ev = drain()
	if ev.Frame.Details.Output == nil || !slices.Contains(ev.Frame.Details.Output.View.Lines, "hello-world") {
		t.Fatalf("expected full command output in DetailOutput, got %+v", ev.Frame.Details.Output)
	}
	detailScreen := renderText(ev.Frame)
	if !strings.Contains(detailScreen, "hello-world") {
		t.Fatalf("rendered Details dialog missing command output:\n%s", detailScreen)
	}

	// Close Details, then open File details for sample.go:3.
	closeID := ev.Frame.Details.View.ID
	_, _ = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{CloseDetails: &closeID})
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Details: &sessionapi.NativeDetailRequest{
			Kind: sessionapi.NativeFileDetails,
			Path: "sample.go:3",
		},
	}); err != nil {
		t.Fatalf("open file details: %v", err)
	}
	ev = drain()
	if ev.Frame.Details == nil || ev.Frame.Details.File == nil || !strings.Contains(ev.Frame.Details.File.Source, "func Hello()") {
		t.Fatalf("expected loaded file details, got %+v", ev.Frame.Details)
	}

	// Close details and open /status panel; verify rendered panel includes account & usage.
	closeID = ev.Frame.Details.View.ID
	_, _ = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{CloseDetails: &closeID})
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/status")}); err != nil {
		t.Fatalf("/status: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Panel == nil || len(ev.Frame.View.Panel.Fields) == 0 {
		t.Fatalf("expected /status PanelView, got nil")
	}
	statusScreen := renderText(ev.Frame)
	for _, want := range []string{"gpt-5", "dev@example.com", "96.0 tok/s", "$0.12"} {
		if !strings.Contains(statusScreen, want) {
			t.Fatalf("rendered /status panel missing %q:\n%s", want, statusScreen)
		}
	}
	// Close panel via op.Panel.
	panelID := ev.Frame.View.Panel.ID
	_, _ = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Panel: &sessionpresentation.PanelAction{Panel: panelID, Kind: sessionpresentation.PanelClose},
	})
	ev = drain()
	if ev.Frame.View.Panel != nil {
		t.Fatalf("expected PanelClose to clear PanelView")
	}

	// /ps opens process dialog from thread/backgroundTerminals/list, and op.StopProcess terminates it.
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/ps")}); err != nil {
		t.Fatalf("/ps: %v", err)
	}
	ev = drain()
	if len(ev.Frame.Processes) != 2 || ev.Frame.Processes[0].Command != "tail -f app.log" || ev.Frame.Processes[1].Agent != "/root/worker" {
		t.Fatalf("expected both family processes in Frame.Processes, got %+v", ev.Frame.Processes)
	}
	if ev.Frame.Details == nil || !ev.Frame.Details.View.Processes {
		t.Fatalf("expected /ps to open Processes detail dialog")
	}
	worker := ev.Frame.Processes[1]
	staleWorker := worker
	staleWorker.Turn = "old-turn"
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		StopProcess: &staleWorker,
	}); !errors.Is(err, sessionpresentation.ErrStaleIntent) {
		t.Fatalf("stale StopProcess should not target a new invocation: %v", err)
	}
	procDialog := ev.Frame.Details.View.ID
	conn.handleNotification(appserver.Message{Method: "item/commandExecution/outputDelta", Params: jsontext.Value(
		`{"threadId":"thr-worker","turnId":"turn-worker","itemId":"cmd-worker","delta":"live worker output\n"}`,
	)})
	ev = drain()
	page := ev.Frame.Details.View.Pages[1]
	if page.Process == nil || !page.Running || !page.Retained || !slices.Contains(page.Tail, "live worker output") {
		t.Fatalf("worker output should reach the open process dialog: %+v", page)
	}
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		StopProcess: &worker,
	}); err != nil {
		t.Fatalf("StopProcess: %v", err)
	}
	ev = drain()
	if len(ev.Frame.Processes) != 1 || ev.Frame.Processes[0].Thread != "thr-main" {
		t.Fatalf("Stop should leave the other thread's process with the same ID, got %+v", ev.Frame.Processes)
	}
	details := ev.Frame.Details
	if details.View.ID != procDialog || len(details.View.Pages) != 2 ||
		details.View.Pages[1].Running || !slices.Contains(details.View.Pages[1].Tail, "live worker output") {
		t.Fatalf("settled worker output should remain in the same process dialog: %+v", details)
	}
	conn.handleNotification(appserver.Message{Method: "item/completed", Params: jsontext.Value(
		`{"threadId":"thr-worker","turnId":"turn-worker","item":{"type":"commandExecution","id":"cmd-worker","command":"tail -f worker.log","status":"failed","processId":"proc-42","exitCode":2,"aggregatedOutput":"live worker output\n"}}`,
	)})
	ev = drain()
	page = ev.Frame.Details.View.Pages[1]
	if !page.Exited || page.ExitCode != 2 {
		t.Fatalf("late native process result should update the retained row: %+v", page)
	}
	srv.mu.Lock()
	srv.bgTerminals = append(srv.bgTerminals, map[string]any{
		"threadId": "thr-worker", "turnId": "turn-next", "itemId": "cmd-next",
		"processId": "proc-53", "command": "sleep 32", "cwd": ws,
	})
	srv.mu.Unlock()
	conn.handleNotification(appserver.Message{Method: "thread/backgroundTerminals/changed", Params: jsontext.Value(
		`{"threadId":"thr-worker"}`,
	)})
awaitProcess:
	for {
		select {
		case ev = <-conn.events:
			if slices.ContainsFunc(ev.Frame.Processes, func(p sessionpresentation.Process) bool { return p.Item == "cmd-next" }) {
				if ev.Frame.Details == nil || ev.Frame.Details.View.ID != procDialog {
					t.Fatalf("a new process should update the open dialog, not replace it: %+v", ev.Frame.Details)
				}
				break awaitProcess
			}
		case <-ctx.Done():
			t.Fatal("the open process dialog missed a native registration notification")
		}
	}

	// /title, /copy, /lock, /unlock, /live, /predictions, /session, /skills, op.Reference, op.Notice, !shell restore, Frame.Live.
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/title Renamed Session")}); err != nil {
		t.Fatalf("/title: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Info.Title != "Renamed Session" {
		t.Fatalf("expected updated title, got %q", ev.Frame.View.Info.Title)
	}

	conn.handleNotification(appserver.Message{Method: "item/completed", Params: jsontext.Value(
		`{"threadId":"thr-main","turnId":"turn-1","item":{"type":"agentMessage","id":"copy-source","text":"Answer.\n\n~~~sh\nprintf '界'  \n~~~\n"}}`,
	)})
	conn.handleNotification(appserver.Message{Method: "item/agentMessage/delta", Params: jsontext.Value(
		`{"threadId":"thr-main","turnId":"turn-2","itemId":"copy-partial","delta":"Not finished"}`,
	)})
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/copy")}); err != nil {
		t.Fatalf("/copy: %v", err)
	}
	ev = drain()
	menu := ev.Frame.View.Menu
	if menu == nil || menu.Kind != sessionpresentation.MenuCopy || len(menu.Choices) != 2 ||
		menu.Choices[0].Name != "Whole response" || menu.Choices[1].Name != "sh code" {
		t.Fatalf("expected whole-response and code choices from the last completed item, got %+v", menu)
	}
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Menu: &sessionpresentation.MenuAction{
		Menu: menu.ID, Kind: sessionpresentation.MenuAccept, Choice: menu.Choices[1].Identity(),
	}}); err != nil {
		t.Fatalf("copy code: %v", err)
	}
	ev = drain()
	foundCopy := false
	for _, eff := range ev.Frame.Effects {
		if eff.Clipboard != nil && *eff.Clipboard == "printf '界'  \n" {
			foundCopy = true
		}
	}
	if !foundCopy {
		t.Fatalf("expected /copy clipboard effect, got %+v", ev.Frame.Effects)
	}

	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/btw First question?")}); err != nil {
		t.Fatal(err)
	}
	ev = drain()
	first := ev.Frame.View.BTW
	if first == nil || first.Question != "First question?" || first.Key != "thr-side" {
		t.Fatalf("side dock did not attach to the returned fork: %+v", first)
	}
	conn.handleNotification(appserver.Message{Method: "item/agentMessage/delta", Params: jsontext.Value(
		`{"threadId":"thr-side","turnId":"btw-turn","itemId":"a1","delta":"The ans"}`,
	)})
	conn.handleNotification(appserver.Message{Method: "item/completed", Params: jsontext.Value(
		`{"threadId":"thr-side","turnId":"btw-turn","item":{"type":"agentMessage","id":"a1","text":"The answer."}}`,
	)})
	conn.handleNotification(appserver.Message{Method: "turn/completed", Params: jsontext.Value(
		`{"threadId":"thr-side","turn":{"id":"btw-turn","status":"completed"}}`,
	)})
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/btw A follow-up?")}); err != nil {
		t.Fatal(err)
	}
	ev = drain()
	followUp := ev.Frame.View.BTW
	if followUp == nil || followUp.Key != first.Key || followUp.Question != "A follow-up?" ||
		len(followUp.History) != 1 || len(followUp.History[0].Answer) != 1 ||
		followUp.History[0].Answer[0].Text != "The answer." {
		t.Fatalf("side follow-up lost the accepted exchange: %+v", followUp)
	}
	closeBTW := func(id uint64) error {
		_, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{BTW: &sessionpresentation.CloseSideIntent{
			Version: ev.Frame.View.Version, Key: followUp.Key, Exchange: id,
		}})
		return err
	}
	if err := closeBTW(first.ID); !errors.Is(err, sessionpresentation.ErrStaleIntent) {
		t.Fatalf("stale side close: %v", err)
	}
	if err := closeBTW(followUp.ID); err != nil {
		t.Fatal(err)
	}
	ev = drain()
	if ev.Frame.View.BTW != nil || ev.Frame.View.Main.ID != "thr-main" {
		t.Fatalf("closing the side answer changed Main: %+v", ev.Frame.View.BTW)
	}

	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/lock")}); err != nil {
		t.Fatalf("/lock: %v", err)
	}
	ev = drain()
	lastEff := ev.Frame.Effects[len(ev.Frame.Effects)-1]
	if lastEff.Presentation == nil || !lastEff.Presentation.InterruptLocked {
		t.Fatalf("expected /lock Presentation effect, got %+v", lastEff)
	}

	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Reference: &sessionpresentation.ReferenceIntent{
			Description: "main.go:1",
			Text:        "package demo",
			Source:      "main.go",
			Target:      conversation.EditorTarget{Revision: 3},
		},
	}); err != nil {
		t.Fatalf("Reference: %v", err)
	}
	ev = drain()
	lastEff = ev.Frame.Effects[len(ev.Frame.Effects)-1]
	if lastEff.Selection == nil || lastEff.Selection.Text != "package demo" {
		t.Fatalf("expected Reference Selection effect, got %+v", lastEff)
	}

	// Dismiss only the notice that was visible for that editor revision.
	notice := func(text string) sessionapi.NativeEvent {
		conn.mu.Lock()
		conn.setFeedbackLocked(text, true)
		conn.publishLocked()
		conn.mu.Unlock()
		return drain()
	}
	old := notice("old error")
	current := notice("new error")
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Notice: &sessionapi.NativeNoticeAction{
			Version: old.Frame.View.Version, Feedback: old.Frame.View.Status.FeedbackRevision, Dismiss: true,
		},
	}); err != nil {
		t.Fatalf("stale Notice dismiss: %v", err)
	}
	conn.mu.Lock()
	feedback := conn.feedbackText
	conn.mu.Unlock()
	if feedback != "new error" {
		t.Fatalf("dismissing the previous notice cleared %q", feedback)
	}
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Notice: &sessionapi.NativeNoticeAction{
			Version: current.Frame.View.Version, Feedback: current.Frame.View.Status.FeedbackRevision, Dismiss: true,
		},
	}); err != nil {
		t.Fatalf("Notice dismiss: %v", err)
	}
	ev = drain()
	if ev.Frame.View.Status.Feedback.Text != "" {
		t.Fatalf("expected Notice dismiss to clear feedback, got %q", ev.Frame.View.Status.Feedback.Text)
	}

	// A rejected shell request restores the draft once at its editor, not also
	// through a retained effect that could offer a duplicate on reconnect.
	srv.mu.Lock()
	srv.shellFailMsg = "shell busy"
	srv.mu.Unlock()
	client := &nativeClient{connection: conn, event: ev, renderer: renderer}
	client.editor("main").Replace(composer.Draft{Text: "!git status"})
	client.composer(terminalsession.ComposerInput{Command: sessionpresentation.ComposerIntent{
		Operation: sessionpresentation.SubmitComposer, Thread: "thr-main",
	}})
	call := client.calls[len(client.calls)-1]
	_, err = conn.Send(ctx, call.target, call.operation)
	if !nativeAdmissionRefused(err) {
		t.Fatalf("expected explicit !shell admission refusal, got %v", err)
	}
	call.after(err)
	if text := client.editor("main").Text; text != "!git status" || len(client.unknown) != 0 {
		t.Fatalf("rejected shell restored %q with %d unknown outcomes", text, len(client.unknown))
	}

	// The patch stream populates Frame.Live before the file-change item completes.
	conn.handleNotification(appserver.Message{
		Method: "turn/started",
		Params: jsontext.Value(`{"threadId":"thr-main","turn":{"id":"turn-2","status":"inProgress"}}`),
	})
	_ = drain()
	patch := appserver.Message{
		Method: "item/fileChange/patchUpdated",
		Params: jsontext.Value(`{"threadId":"thr-main","turnId":"turn-2","itemId":"edit-1","changes":[{"path":"a.go","kind":{"type":"add"},"diff":"new\n"}]}`),
	}
	conn.handleNotification(patch)
	ev = drain()
	if len(ev.Frame.Live) != 1 || len(ev.Frame.Live[0].Files) != 1 || ev.Frame.Live[0].Files[0].Diff != "@@ -0,0 +1,1 @@\n+new\n" {
		t.Fatalf("expected Frame.Live from patchUpdated, got %+v", ev.Frame.Live)
	}
	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{"threadId":"thr-main","turnId":"turn-2","item":{"type":"fileChange","id":"edit-1","status":"completed","changes":[]}}`),
	})
	conn.handleNotification(patch)
	ev = drain()
	if len(ev.Frame.Live) != 0 {
		t.Fatalf("expected completed patch to stay cleared, got %+v", ev.Frame.Live)
	}
}

func TestUISnapshotNativeAgentMail(t *testing.T) {
	f := newNativeParitySnapshot(t)
	f.client.renderer.views.ChildReplySummary = true
	const summary = "The parser checks pass."
	const supporting = "The supporting work stays in the reply."
	const source = "## Summary\n" + summary + "\n\n## Details\n" + supporting
	const shell = `{"type":"commandExecution","id":"shell","command":"printf checked","status":"completed","exitCode":0}`
	const mainReply = `{"type":"agentMessage","id":"main-answer","text":"I reviewed the result."}`
	const childMail = `{"type":"agentMail","id":"message","kind":"message","sender":"/root","recipient":"/root/worker","text":"Please include the supporting work.","summary":null,"details":null}`
	reply := fmt.Sprintf(`{"type":"agentMail","id":"delivered","kind":"reply","sender":"/root/worker","recipient":"/root","text":%q,"summary":%q,"details":%q}`,
		source, summary, supporting)
	childReply := fmt.Sprintf(`{"type":"agentMessage","id":"child-answer","text":%q}`, source)
	notify := func(thread, turn, item string) {
		t.Helper()
		f.notify("item/completed", fmt.Sprintf(`{"threadId":%q,"turnId":%q,"item":%s}`, thread, turn, item))
	}
	notify("thr-main", "turn-1", shell)
	notify("thr-main", "turn-1", reply)
	notify("thr-main", "turn-1", mainReply)
	live := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.Main != nil && len(ev.Frame.Main.Entries) == 3
	})
	const width, height = 88, 20
	f.assert("agent-mail", live, width, height)
	u := f.client.renderer.terminal
	replySeq := live.Frame.Main.Entries[1].Seq
	row := slices.Index(u.Transcript.FeedQuestions, replySeq)
	if row < 0 {
		t.Fatal("the painted reply cannot be opened")
	}
	x := u.Layout.Main.X + u.Transcript.FeedLeft + 2 // Past the gutter.
	y := u.Layout.Main.Y + u.Transcript.FeedTop + row
	for _, key := range []string{
		fmt.Sprintf("\x1b[<0;%d;%dM", x, y),
		fmt.Sprintf("\x1b[<0;%d;%dm", x, y),
	} {
		f.client.dispatch(terminalsession.Input{Kind: terminalsession.MouseInput, Text: key})
	}
	for _, call := range f.client.calls {
		if _, err := f.conn.Send(f.ctx, call.target, call.operation); err != nil {
			t.Fatal(err)
		}
	}
	f.client.calls = nil
	details := f.await(func(ev sessionapi.NativeEvent) bool { return ev.Frame.Details != nil })
	f.assert("agent-mail-details", details, width, height)
	if u.Dialog.Document.Text != source || len(u.Dialog.Pages) != 1 {
		t.Fatal("Open reply did not resolve the original delivered Markdown")
	}
	f.send(sessionapi.NativeOperation{CloseDetails: &details.Frame.Details.View.ID})

	f.server.mu.Lock()
	f.server.overrides["thread/read"] = func(params jsontext.Value) any {
		if strings.Contains(string(params), "thr-worker") {
			return jsontext.Value(`{"thread":{"id":"thr-worker","turns":[{"id":"child-turn","items":[` + childMail + "," + childReply + `]}]}}`)
		}
		return jsontext.Value(`{"thread":{"id":"thr-main","turns":[{"id":"turn-1","items":[` + shell + "," + reply + "," + mainReply + `]}]}}`)
	}
	f.server.mu.Unlock()
	f.send(sessionapi.NativeOperation{Display: &sessionpresentation.DisplayIntent{
		Thread: sessionpresentation.Thread{ID: "thr-worker", Caller: "/root/worker"},
	}})
	child := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Displayed.ID == "thr-worker" && ev.Frame.Agents != nil &&
			len(ev.Frame.Agents.Entries) == 2 && ev.Frame.Agents.Entries[0].Restored
	})
	f.assert("agent-mail-child", child, width, height)
	f.send(sessionapi.NativeOperation{Display: &sessionpresentation.DisplayIntent{
		Thread: sessionpresentation.Thread{ID: "thr-main", Caller: "/root"},
	}})
	retained := f.await(func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Displayed.ID == "thr-main" && ev.Frame.Main != nil &&
			len(ev.Frame.Main.Entries) == 3 && ev.Frame.Main.Entries[1].Restored
	})
	f.assert("agent-mail", retained, width, height)
}

func TestUISnapshotDirectNativeProjection(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "native.sock")
	srv := startMockNativeAppServer(t, socket)
	srv.mu.Lock()
	srv.overrides["thread/start"] = func(jsontext.Value) any {
		return map[string]any{
			"thread": map[string]any{"id": "thr-main", "cwd": "/workspace/demo", "name": "Direct Session"},
			"model":  "gpt-5", "modelProvider": "preview", "reasoningEffort": "medium", "serviceTier": "standard",
			"approvalPolicy": "on-request", "sandbox": map[string]any{"type": "workspaceWrite"},
			"instructionSources": []string{"/workspace/demo/AGENTS.md"},
		}
	}
	srv.overrides["account/rateLimits/read"] = func(jsontext.Value) any {
		return jsontext.Value(`{
			"rateLimitsByLimitId": {
				"codex": {"limitName":"Codex","primary":{"usedPercent":12,"windowDurationMins":300},
					"secondary":{"usedPercent":21,"windowDurationMins":10080},"credits":{"balance":"12.50"}},
				"fast": {"limitName":"Fast","primary":{"usedPercent":40,"windowDurationMins":60,
					"resetsAt":1795694400},"credits":{"unlimited":true}}
			},
			"rateLimitResetCredits": {"availableCount":2,"credits":[
				{"status":"available","resetType":"codexRateLimits"},{"status":"used","title":"Used reset"}
			]}
		}`)
	}
	srv.overrides["thread/changes/read"] = func(jsontext.Value) any {
		return jsontext.Value(`{"threadId":"thr-main","workspace":"/workspace/demo","changes":[],"files":[]}`)
	}
	srv.mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	conn, err := openDirectNativeConnection(ctx, socket, "/workspace/demo", nil, nil, "", "")
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

	now := time.Date(2026, 11, 26, 12, 0, 0, 0, time.Local)
	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{
			"threadId":"thr-main","turnId":"turn-1",
			"item":{
				"type":"commandExecution","id":"cmd-1",
				"command":"/bin/bash -lc 'go test ./internal/launch'",
				"display":{"script":"go test ./internal/launch","segments":[],"effectiveCwd":null},
				"status":"completed","exitCode":0,
				"aggregatedOutput":"ok 1\nok 2\nok 3\nok 4\nok 5\nPASS\n"
			}
		}`),
	})
	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{
			"threadId":"thr-main","turnId":"turn-1",
			"item":{
				"type":"fileChange","id":"fc-1","status":"completed",
				"changes":[{"path":"main.go","kind":{"type":"update"},"diff":"@@ -1 +1 @@\n-old\n+new\n"}]
			}
		}`),
	})
	conn.handleNotification(appserver.Message{
		Method: "item/completed",
		Params: jsontext.Value(`{
			"threadId":"thr-main","turnId":"turn-1",
			"item":{"type":"agentMessage","id":"msg-1","text":"Fixed native TUI projection."}
		}`),
	})
	conn.handleNotification(appserver.Message{
		Method: "thread/tokenUsage/updated",
		Params: jsontext.Value(`{
			"threadId":"thr-main",
			"tokenUsage":{
				"total":{"totalTokens":15000,"inputTokens":12000,"outputTokens":3000,"cachedInputTokens":1000,"reasoningOutputTokens":200},
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
				"total":{"totalTokens":5000,"inputTokens":4000,"outputTokens":1000,"cachedInputTokens":0,"reasoningOutputTokens":0},
				"last":{"totalTokens":10000},
				"modelContextWindow":200000,
				"estimatedCostUsd":0.36,
				"costPartial":false,
				"outputTokensPerSecond":64.0,
				"workDurationMs":800,
				"roundtrips":1
			}
		}`),
	})
	// Pin entry timestamps for deterministic snapshots.
	conn.mu.Lock()
	for _, rec := range conn.items["thr-main"] {
		rec.entry.Observed = now
		if !rec.entry.Completed.IsZero() {
			rec.entry.Completed = now
		}
	}
	conn.publishLocked()
	conn.mu.Unlock()
	ev := drain()

	views, err := composition.NativeRenderer(false)
	if err != nil {
		t.Fatalf("NativeRenderer: %v", err)
	}
	terminal, _ := views.NewAttachment(diffrender.DarkTheme)
	terminal.Width, terminal.Height = 140, 24
	renderer := nativeClientRenderer{terminal: terminal, views: views}

	renderFrame := func(frame sessionapi.NativeFrame) string {
		renderer.update(frame, conversation.DraftView{}, conversation.PromptDraftView{}, now)
		in := terminal.FrameInput(now, "0.172.0")
		if frame.Family != nil {
			in.FamilyRoot = frame.Family.Root
		}
		rendered, err := terminal.Frame.Render(ctx, in)
		if err != nil {
			t.Fatalf("Frame.Render: %v", err)
		}
		if len(rendered.Demands) > 0 {
			if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
				Output: &sessionapi.NativeOutputDemand{Demands: rendered.Demands},
			}); err != nil {
				t.Fatalf("satisfy output demands: %v", err)
			}
			ev = drain()
			renderer.update(ev.Frame, conversation.DraftView{}, conversation.PromptDraftView{}, now)
			in = terminal.FrameInput(now, "0.172.0")
			if ev.Frame.Family != nil {
				in.FamilyRoot = ev.Frame.Family.Root
			}
			rendered, err = terminal.Frame.Render(ctx, in)
			if err != nil {
				t.Fatalf("Frame.Render after hydration: %v", err)
			}
		}
		return strings.Join(rendered.Rows, "\n")
	}

	// 1. Default compact transcript + caption with cost, tok/s, and rate limits.
	uisnapshot.Assert(t, "testdata/snapshots/direct-native-compact-140.txt", renderFrame(ev.Frame))

	// 2. ctrl+e expanded view (Expansion == 2) with full hydrated command output.
	terminal.Transcript.ToggleExpansion()
	terminal.Transcript.ToggleExpansion()
	uisnapshot.Assert(t, "testdata/snapshots/direct-native-expanded-140.txt", renderFrame(ev.Frame))
	terminal.Transcript.ToggleExpansion()

	// 3. Entry Details dialog with hydrated DetailOutput.
	cmdSeq := conn.itemsByID[directItemKey{"thr-main", "cmd-1"}].seq
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		Details: &sessionapi.NativeDetailRequest{Kind: sessionapi.NativeEntryDetails, Source: cmdSeq},
	}); err != nil {
		t.Fatalf("open details: %v", err)
	}
	ev = drain()
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{
		DetailOutput: &sessionpresentation.DetailOutputRequest{
			ID:      ev.Frame.Details.View.ID,
			Page:    ev.Frame.Details.View.Pages[0].ID,
			Version: ev.Frame.Details.View.Pages[0].OutputVersion,
		},
	}); err != nil {
		t.Fatalf("detail output: %v", err)
	}
	ev = drain()
	uisnapshot.Assert(t, "testdata/snapshots/direct-native-details-140.txt", renderFrame(ev.Frame))
	closeID := ev.Frame.Details.View.ID
	_, _ = conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{CloseDetails: &closeID})

	// 4. /status panel.
	if _, err := conn.Send(ctx, sessionapi.NativeTarget{}, sessionapi.NativeOperation{Command: new("/status")}); err != nil {
		t.Fatalf("/status: %v", err)
	}
	ev = drain()
	if p := ev.Frame.View.Panel; p == nil ||
		!slices.ContainsFunc(p.Fields, func(row sessionpresentation.StatusField) bool {
			return row.Label == "Fast 1h limit" && row.Value == "60% left" && strings.HasPrefix(row.Detail, "resets ")
		}) || !slices.ContainsFunc(p.Fields, func(row sessionpresentation.StatusField) bool {
		return row.Label == "Fast credits" && row.Value == "unlimited"
	}) || !slices.ContainsFunc(p.Fields, func(row sessionpresentation.StatusField) bool {
		return row.Group == "Usage limit resets" && row.Label == "Available" && row.Value == "2"
	}) || !slices.ContainsFunc(p.Fields, func(row sessionpresentation.StatusField) bool {
		return row.Label == "Full reset" && row.Value == "Does not expire"
	}) || slices.ContainsFunc(p.Fields, func(row sessionpresentation.StatusField) bool {
		return row.Label == "Used reset"
	}) {
		t.Fatalf("/status did not show the account buckets, credits and reset inventory: %#v", p)
	}
	uisnapshot.Assert(t, "testdata/snapshots/direct-native-status-140.txt", renderFrame(ev.Frame))
}
