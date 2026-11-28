package launch

import (
	"io"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/ansi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/composition"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffrender"
	conversation "github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/presentation"
)

func TestDirectNativeLivePreviewFollowsDisplayedThread(t *testing.T) {
	socket := filepath.Join(shortTempDir(t), "native.sock")
	server := startMockNativeAppServer(t, socket)
	ctx := t.Context()
	conn, err := openDirectNativeConnection(ctx, socket, t.TempDir(), nil, nil, "", "")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
		return ev.Frame.View.Main.ID == "thr-main" && ev.Frame.Family != nil
	})

	views, err := composition.NativeRenderer(false)
	if err != nil {
		t.Fatal(err)
	}
	terminal, _ := views.NewAttachment(diffrender.DarkTheme)
	terminal.Width, terminal.Height = 120, 40
	renderer := nativeClientRenderer{terminal: terminal, views: views}
	now := time.Date(2026, 11, 26, 12, 0, 0, 0, time.UTC)
	paint := func(ev sessionapi.NativeEvent) string {
		t.Helper()
		publishedAt := now
		now = now.Add(time.Second)
		for hydrated := false; ; hydrated = true {
			renderer.update(ev.Frame, conversation.DraftView{}, conversation.PromptDraftView{}, publishedAt)
			rendered, err := renderer.render(ctx, io.Discard, now, "0.172.0")
			if err != nil {
				t.Fatal(err)
			}
			if len(rendered.Demands) == 0 {
				dock := terminal.Shell.Layout.Live
				return ansi.Strip(strings.Join(rendered.Rows[dock.Y:dock.Y+dock.H], "\n"))
			}
			if hydrated {
				t.Fatalf("output demands remained after hydration: %v", rendered.Demands)
			}
			// The attached transcript must finish hydrating before the renderer paints the dock.
			agent := ev.Frame.View.Displayed.ID != ev.Frame.View.Main.ID
			if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
				Output: &sessionapi.NativeOutputDemand{Agent: agent, Demands: rendered.Demands},
			}); err != nil {
				t.Fatal(err)
			}
			ev = awaitDirectEvent(t, conn, func(next sessionapi.NativeEvent) bool {
				output := next.Frame.MainOutput
				if agent {
					output = next.Frame.AgentOutput
				}
				return next.Frame.View.Displayed.ID == ev.Frame.View.Displayed.ID &&
					next.Frame.View.Version.Revision > ev.Frame.View.Version.Revision &&
					len(output) > 0 && output[0].Demand == rendered.Demands[0]
			})
		}
	}
	startEdit := func(thread, path, text string) sessionapi.NativeEvent {
		t.Helper()
		server.send(t, map[string]any{
			"jsonrpc": "2.0", "method": "item/started",
			"params": map[string]any{
				"threadId": thread, "turnId": "turn-" + thread,
				"item": map[string]any{
					"type": "fileChange", "id": "edit-" + thread, "status": "inProgress",
					"changes": []map[string]any{{
						"path": path, "kind": map[string]string{"type": "update"},
						"diff": "@@ -1 +1 @@\n-old\n+" + text + "\n",
					}},
				},
			},
		})
		return awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
			for _, edit := range ev.Frame.Live {
				if edit.Thread == thread && len(edit.Files) == 1 && strings.Contains(edit.Files[0].Diff, text) {
					return true
				}
			}
			return false
		})
	}
	display := func(ev sessionapi.NativeEvent, id, caller string) sessionapi.NativeEvent {
		t.Helper()
		if _, err := conn.Send(ctx, ev.Target(), sessionapi.NativeOperation{
			Display: &sessionpresentation.DisplayIntent{Thread: sessionpresentation.Thread{ID: id, Caller: caller}},
		}); err != nil {
			t.Fatal(err)
		}
		return awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
			return ev.Frame.View.Displayed.ID == id
		})
	}

	server.send(t, map[string]any{
		"jsonrpc": "2.0", "method": "turn/started",
		"params": map[string]any{
			"threadId": "thr-main", "turn": map[string]any{"id": "turn-thr-main", "status": "inProgress"},
		},
	})
	for _, tc := range []struct {
		name, kind, diff string
		numbered         bool
		want             []string
	}{
		{
			name: "shell replacements", kind: "update",
			diff: "@@ -1,1 +1,1 @@\n-first old\n+first new\n@@ -1,1 +1,1 @@\n-second old\n+second new\n",
			want: []string{"+2 -2", "+first new", "+second new"},
		},
		{
			name: "apply_patch anchors", kind: "update",
			diff: "@@\n--- old text\n+++ new text\n@@ func example\n-other old\n+other new\n",
			want: []string{"+2 -2", "+++ new text", "+other new"},
		},
		{
			name: "resolved preview", kind: "update", numbered: true,
			diff: "@@ -137 +137 @@\n-old\n+new\n",
			want: []string{"+1 -1", "137│+new"},
		},
		{
			name: "whole file addition", kind: "add", numbered: true,
			diff: "first new line\nsecond new line",
			want: []string{"+2 -0", "1│+first new line", "2│+second new line"},
		},
	} {
		server.send(t, map[string]any{
			"jsonrpc": "2.0", "method": "item/fileChange/patchUpdated",
			"params": map[string]any{
				"threadId": "thr-main", "turnId": "turn-thr-main", "itemId": "edit-thr-main",
				"changes": []map[string]any{{
					"path": "main.go", "kind": map[string]string{"type": tc.kind}, "diff": tc.diff,
				}},
			},
		})
		event := awaitDirectEvent(t, conn, func(ev sessionapi.NativeEvent) bool {
			if len(ev.Frame.Live) != 1 || len(ev.Frame.Live[0].Files) != 1 {
				return false
			}
			file := ev.Frame.Live[0].Files[0]
			return file.Diff == tc.diff || tc.kind == "add" && file.BeforePath == ""
		})
		shown := paint(event)
		for _, want := range tc.want {
			if !strings.Contains(shown, want) {
				t.Fatalf("%s: expected %q in live diff:\n%s", tc.name, want, shown)
			}
		}
		if !tc.numbered && strings.Contains(shown, "│+") {
			t.Fatalf("%s: live diff invented source coordinates:\n%s", tc.name, shown)
		}
	}

	main := startEdit("thr-main", "main.go", "main preview hunk")
	if shown := paint(main); !strings.Contains(shown, "1│+main preview hunk") {
		t.Fatalf("Main live diff did not survive resync:\n%s", shown)
	}
	worker := display(main, "thr-worker", "/root/worker")
	if shown := paint(worker); strings.Contains(shown, "main preview hunk") || terminal.Live.Live() != 0 {
		t.Fatalf("selected worker retained Main's live diff:\n%s", shown)
	}
	worker = startEdit("thr-worker", "worker.go", "worker preview hunk")
	if shown := paint(worker); !strings.Contains(shown, "worker preview hunk") {
		t.Fatalf("selected worker's live diff did not survive resync:\n%s", shown)
	}
	if shown := paint(display(worker, "thr-main", "/root")); terminal.Live.Live() != 2 {
		t.Fatalf("Main did not retain both admitted live diffs:\n%s", shown)
	}
}
