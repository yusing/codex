package launch

import (
	"cmp"
	"context"
	"encoding/json/jsontext"
	json "encoding/json/v2"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/appserver"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/binding"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffmodel"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/textsanitize"
	activityfeed "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/feed"
	activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/prompt"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/livepreview/preview"
)

const directCommandTailLimit = 3

func directCommandTail(output string) ([]string, int) {
	if output == "" {
		return nil, 0
	}
	lines := strings.Split(strings.TrimSuffix(output, "\n"), "\n")
	omitted := max(0, len(lines)-directCommandTailLimit)
	lines = lines[omitted:]
	for i, line := range lines {
		lines[i] = textsanitize.Safe(directOutputLine(line), false)
	}
	return lines, omitted
}

func directOutputLines(output string) []string {
	if output == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(output, "\n"), "\n")
	for i, line := range lines {
		lines[i] = textsanitize.MarkdownSource(directOutputLine(line))
	}
	return lines
}

func directOutputLine(line string) string {
	// CRLF ends the line; an earlier CR replaces the progress text on that line.
	line = strings.TrimSuffix(line, "\r")
	if at := strings.LastIndexByte(line, '\r'); at >= 0 {
		line = line[at+1:]
	}
	return line
}

func (c *directNativeConnection) handleServerMessage(msg appserver.Message) {
	if len(msg.ID) > 0 && msg.Method != "" {
		c.handleServerRequest(msg)
		return
	}
	c.handleNotification(msg)
}

func (c *directNativeConnection) setActiveInputTurnLocked(threadID, turnID string, startedAt *int64) {
	var since time.Time
	if startedAt != nil {
		since = time.Unix(*startedAt, 0)
	}
	if threadID == c.sideThread {
		if c.sideActiveTurn != turnID || !since.IsZero() {
			c.sideTurnSince = since
		}
		c.sideActiveTurn = turnID
		if c.sideFirstTurn == "" {
			c.sideFirstTurn = turnID
		}
	} else {
		if c.activeTurn != turnID || !since.IsZero() {
			c.activeTurnSince = since
		}
		c.activeTurn = turnID
		c.resetPredictionLocked("")
	}
	c.busy = true
}

func directFinishedTurnStatus(status string, durationMS *int64) sessionpresentation.Label {
	switch status {
	case "completed":
		label := sessionpresentation.Label{Text: "Completed"}
		if durationMS != nil {
			label.Completion, label.CompletionKnown = time.Duration(*durationMS)*time.Millisecond, true
		}
		return label
	case "interrupted":
		return sessionpresentation.Label{Text: "Interrupted"}
	case "failed":
		return sessionpresentation.Label{Text: "Failed", Alert: true}
	}
	return sessionpresentation.Label{}
}

func (c *directNativeConnection) handleNotification(msg appserver.Message) {
	switch msg.Method {
	case "turn/started":
		var p struct {
			ThreadID string `json:"threadId"`
			Turn     struct {
				ID        string `json:"id"`
				StartedAt *int64 `json:"startedAt"`
			} `json:"turn"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			switch cmp.Or(p.ThreadID, c.mainThread) {
			case c.btwThread:
				c.btwRunning = true
				c.btwRetrying = false
			case c.sideThread:
				c.setActiveInputTurnLocked(c.sideThread, p.Turn.ID, p.Turn.StartedAt)
			default:
				if p.ThreadID == "" || p.ThreadID == c.mainThread {
					c.setActiveInputTurnLocked(c.mainThread, p.Turn.ID, p.Turn.StartedAt)
				}
			}
			c.clearRetryNoticeLocked(cmp.Or(p.ThreadID, c.mainThread), "")
			c.publishLocked()
			c.mu.Unlock()
		}

	case "error":
		var p struct {
			ThreadID  string             `json:"threadId"`
			TurnID    string             `json:"turnId"`
			Error     *binding.TurnError `json:"error"`
			WillRetry bool               `json:"willRetry"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			threadID := cmp.Or(p.ThreadID, c.mainThread)
			if p.WillRetry {
				if diagnostic := directTurnErrorText(p.Error); diagnostic != "" {
					if p.ThreadID != "" && p.ThreadID == c.btwThread {
						c.btw.Status, c.btw.Alert = diagnostic, false
						c.btwRetrying = true
						c.publishLocked()
					} else if threadID == c.mainThread || threadID == c.sideThread {
						if c.retryNotices == nil {
							c.retryNotices = make(map[string]directRetryNotice)
						}
						c.retryNotices[threadID] = directRetryNotice{threadID: threadID, turnID: p.TurnID, text: diagnostic}
						c.publishLocked()
					}
				}
			} else {
				cleared := c.clearRetryNoticeLocked(threadID, p.TurnID)
				projected := p.ThreadID != c.btwThread && c.projectTurnErrorLocked(p.ThreadID, p.TurnID, p.Error) != ""
				if cleared || projected {
					c.publishLocked()
				}
			}
			c.mu.Unlock()
		}

	case "turn/completed":
		var p struct {
			ThreadID string       `json:"threadId"`
			Turn     binding.Turn `json:"turn"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			threadID := cmp.Or(p.ThreadID, c.mainThread)
			c.finishSteerTurnLocked(directSteerTurnID{thread: threadID, turn: p.Turn.ID})
			if first, ok := c.firstInputs[threadID]; ok && first.turnID == p.Turn.ID {
				delete(c.firstInputs, threadID)
			}
			for _, start := range c.inputTurnStarts {
				if start.threadID == threadID {
					start.completed = append(start.completed, p.Turn.ID)
				}
			}
			if p.ThreadID != "" && p.ThreadID == c.mainThread &&
				p.Turn.Status == "completed" && p.Turn.ID != c.prediction.turn &&
				(c.activeTurn == "" || p.Turn.ID == c.activeTurn) {
				c.resetPredictionLocked(p.Turn.ID)
			}
			switch threadID {
			case c.btwThread:
				c.btwRunning = false
				c.btwRetrying = false
				c.btw.Status, c.btw.Alert = p.Turn.Status, p.Turn.Status == "failed"
				if p.Turn.Status == "completed" && len(c.btw.Answer) != 0 {
					c.btwCompletedAnswer = c.btw.Answer[len(c.btw.Answer)-1].Text
				} else if p.Turn.Error != nil && p.Turn.Error.Message != "" {
					c.btw.Status = p.Turn.Error.Message
				}
			case c.sideThread:
				if c.sideActiveTurn == "" || c.sideActiveTurn == p.Turn.ID {
					c.sideActiveTurn = ""
					c.sideTurnStatus = directFinishedTurnStatus(p.Turn.Status, p.Turn.DurationMS)
				}
				c.busy = c.activeTurn != "" || c.sideActiveTurn != ""
			case c.mainThread:
				if c.activeTurn == "" || c.activeTurn == p.Turn.ID {
					c.activeTurn = ""
					c.mainTurnStatus = directFinishedTurnStatus(p.Turn.Status, p.Turn.DurationMS)
				}
				c.busy = c.activeTurn != "" || c.sideActiveTurn != ""
			}
			if threadID != c.btwThread {
				diagnostic := c.projectTurnErrorLocked(threadID, p.Turn.ID, p.Turn.Error)
				if diagnostic != "" && (threadID == c.mainThread || threadID == c.sideThread) {
					c.setFeedbackLocked(diagnostic, true)
				}
			}
			c.clearRetryNoticeLocked(threadID, p.Turn.ID)
			c.removeNativePreviewsLocked(func(edit preview.Preview) bool {
				return edit.Turn == p.Turn.ID && (p.ThreadID == "" || edit.Thread == p.ThreadID)
			})
			mainID := c.mainThread
			c.publishLocked()
			c.mu.Unlock()
			if mainID != "" {
				go c.refreshAuxiliary(c.ctx, mainID)
			}
		}

	case "item/started", "item/completed":
		var p struct {
			ThreadID string         `json:"threadId"`
			TurnID   string         `json:"turnId"`
			Item     jsontext.Value `json:"item"`
		}
		if json.Unmarshal(msg.Params, &p) == nil && len(p.Item) > 0 {
			complete := msg.Method == "item/completed"
			c.mu.Lock()
			threadID := p.ThreadID
			if threadID == "" {
				threadID = c.mainThread
			}
			itemType := c.upsertWireItemLocked(threadID, p.TurnID, p.Item, complete)
			if directItemIsResponseProgress(p.Item, complete) {
				c.clearRetryNoticeLocked(threadID, p.TurnID)
			}
			mainID := c.mainThread
			displayed := cmp.Or(c.displayedThread, mainID)
			c.publishLocked()
			c.mu.Unlock()
			if complete && mainID != "" {
				switch itemType {
				case "fileChange", "commandExecution":
					if threadID == displayed || displayed == mainID {
						c.requestChanges(displayed)
					}
				case "collabAgentToolCall", "subAgentActivity":
					go c.refreshSubtree(c.ctx, mainID)
				}
			}
		}

	case "item/agentMessage/delta":
		var p struct {
			ThreadID string `json:"threadId"`
			TurnID   string `json:"turnId"`
			ItemID   string `json:"itemId"`
			Delta    string `json:"delta"`
		}
		if json.Unmarshal(msg.Params, &p) == nil && p.Delta != "" {
			c.mu.Lock()
			if p.ThreadID != "" && p.ThreadID == c.btwThread {
				c.btw.Append(p.ItemID, p.Delta, false)
				if c.btwRetrying && c.btwRunning && !c.btw.Alert {
					c.btw.Status = "Running"
					c.btwRetrying = false
				}
			} else {
				threadID := p.ThreadID
				if threadID == "" {
					threadID = c.mainThread
				}
				c.clearRetryNoticeLocked(threadID, p.TurnID)
				msgKind := "final"
				if threadID == c.mainThread {
					msgKind = "text"
				}
				rec := c.ensureItemLocked(threadID, p.TurnID, p.ItemID, "agentMessage", msgKind)
				rec.entry.Text += p.Delta
				rec.entry.Live = true
				rec.entry.Running = true
				refreshProjectedValues(rec)
				rec.entry.Revision++
			}
			c.publishLocked()
			c.mu.Unlock()
		}

	case "item/reasoning/summaryPartAdded":
		var p struct {
			ThreadID string `json:"threadId"`
			TurnID   string `json:"turnId"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			if c.clearRetryNoticeLocked(cmp.Or(p.ThreadID, c.mainThread), p.TurnID) {
				c.publishLocked()
			}
			c.mu.Unlock()
		}

	case "item/reasoning/summaryTextDelta":
		var p struct {
			ThreadID     string `json:"threadId"`
			TurnID       string `json:"turnId"`
			ItemID       string `json:"itemId"`
			Delta        string `json:"delta"`
			SummaryIndex int    `json:"summaryIndex"`
		}
		if json.Unmarshal(msg.Params, &p) == nil && p.Delta != "" && p.SummaryIndex >= 0 {
			c.mu.Lock()
			threadID := cmp.Or(p.ThreadID, c.mainThread)
			rec := c.ensureItemLocked(threadID, p.TurnID, p.ItemID, "reasoning", "reasoning")
			if !rec.entry.ItemComplete {
				c.clearRetryNoticeLocked(threadID, p.TurnID)
				for len(rec.reasoningSummary) <= p.SummaryIndex {
					rec.reasoningSummary = append(rec.reasoningSummary, "")
				}
				rec.reasoningSummary[p.SummaryIndex] += p.Delta
				rec.entry.Live = true
				rec.entry.Running = true
				refreshProjectedValues(rec)
				rec.entry.Revision++
				c.publishLocked()
			}
			c.mu.Unlock()
		}

	case "item/commandExecution/outputDelta", "item/fileChange/outputDelta":
		var p struct {
			ThreadID string `json:"threadId"`
			TurnID   string `json:"turnId"`
			ItemID   string `json:"itemId"`
			Delta    string `json:"delta"`
		}
		if json.Unmarshal(msg.Params, &p) == nil && p.Delta != "" {
			c.mu.Lock()
			threadID := p.ThreadID
			if threadID == "" {
				threadID = c.mainThread
			}
			rec := c.ensureItemLocked(threadID, p.TurnID, p.ItemID, "commandExecution", "tool")
			rec.output += p.Delta
			if len(rec.entry.Values) > 0 {
				result := &rec.entry.Values[0]
				if rec.itemType == "commandExecution" {
					result = &rec.entry.Values[len(rec.entry.Values)-1]
				}
				result.Output = 1
				result.Tail, result.TailOmitted = directCommandTail(rec.output)
			}
			if rec.entry.Outputs == nil {
				rec.entry.Outputs = map[int]int{}
			}
			rec.entry.Outputs[1]++
			rec.entry.Revision++
			c.refreshCommandDetailsLocked(rec)
			c.updateProcessDetailsLocked()
			c.publishLocked()
			c.mu.Unlock()
		}

	case "thread/tokenUsage/updated":
		var p struct {
			ThreadID string          `json:"threadId"`
			Usage    directWireUsage `json:"tokenUsage"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			if target := cmp.Or(p.ThreadID, c.mainThread); target != "" {
				c.applyThreadUsageLocked(target, &p.Usage)
				c.publishLocked()
			}
			c.mu.Unlock()
		}

	case "thread/skills/updated":
		var p struct {
			ThreadID string   `json:"threadId"`
			Names    []string `json:"names"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			c.observeThreadSkillsLocked(p.ThreadID, p.Names)
			c.publishLocked()
			c.mu.Unlock()
		}

	case "account/rateLimits/updated":
		c.mu.Lock()
		if directObserveRateLimits(&c.rateLimits, msg.Params) == nil {
			c.publishLocked()
		}
		c.mu.Unlock()

	case "thread/name/updated":
		var p struct {
			ThreadID   string `json:"threadId"`
			ThreadName string `json:"threadName"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			if p.ThreadID == c.mainThread && p.ThreadName != "" {
				c.title = p.ThreadName
				c.publishLocked()
			}
			c.mu.Unlock()
		}

	case "thread/settings/updated":
		var p struct {
			ThreadID       string `json:"threadId"`
			ThreadSettings *struct {
				Cwd                     string                   `json:"cwd"`
				ActivePermissionProfile *directPermissionProfile `json:"activePermissionProfile"`
				ApprovalPolicy          jsontext.Value           `json:"approvalPolicy"`
				SandboxPolicy           directSandboxPolicy      `json:"sandboxPolicy"`
				Model                   string                   `json:"model"`
				ModelProvider           string                   `json:"modelProvider"`
				Effort                  *string                  `json:"effort"`
				ServiceTier             *string                  `json:"serviceTier"`
			} `json:"threadSettings"`
		}
		if json.Unmarshal(msg.Params, &p) != nil || p.ThreadSettings == nil {
			break
		}
		s := p.ThreadSettings
		c.mu.Lock()
		switch p.ThreadID {
		case c.mainThread:
			c.workspace = cmp.Or(s.Cwd, c.workspace)
			c.permissionProfile = ""
			if s.ActivePermissionProfile != nil {
				c.permissionProfile = s.ActivePermissionProfile.ID
			}
			if len(s.ApprovalPolicy) > 0 {
				c.approvalPolicy = directApprovalLabel(s.ApprovalPolicy)
			}
			c.sandboxPolicy = cmp.Or(s.SandboxPolicy.Type, c.sandboxPolicy)
			c.model = cmp.Or(s.Model, c.model)
			if s.ModelProvider != "" && s.ModelProvider != c.modelProvider {
				c.modelProvider = s.ModelProvider
				c.resetPredictionLocked(c.prediction.turn)
			}
			c.effort = ""
			if s.Effort != nil {
				c.effort = *s.Effort
			}
			c.tier = ""
			if s.ServiceTier != nil {
				c.tier = *s.ServiceTier
			}
			c.publishLocked()
		case c.sideThread:
			c.sideModel = cmp.Or(s.Model, c.sideModel)
			c.sideEffort = ""
			if s.Effort != nil {
				c.sideEffort = *s.Effort
			}
			c.publishLocked()
		}
		c.mu.Unlock()

	case "skills/changed":
		c.mu.Lock()
		c.invalidateSkillsLocked()
		c.publishLocked()
		c.mu.Unlock()

	case "thread/queue/changed":
		var p struct {
			ThreadID string `json:"threadId"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			target := p.ThreadID
			if target == "" {
				target = c.mainThread
			}
			hasQueue := c.queueForThreadLocked(target) != nil
			c.mu.Unlock()
			if hasQueue && c.wire != nil {
				go c.refreshQueue(c.ctx, target)
			}
		}

	case "item/fileChange/patchUpdated":
		var p struct {
			ThreadID string             `json:"threadId"`
			TurnID   string             `json:"turnId"`
			ItemID   string             `json:"itemId"`
			Changes  []nativeFileChange `json:"changes"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			threadID := cmp.Or(p.ThreadID, c.mainThread)
			trackedTurn := threadID == c.mainThread || threadID == c.sideThread
			item := c.itemsByID[directItemKey{threadID, p.ItemID}]
			if (!trackedTurn || c.activeTurnForLocked(threadID) == p.TurnID) &&
				(item == nil || item.entry.Completed.IsZero()) {
				c.setNativeFilePreviewLocked(threadID, p.TurnID, p.ItemID, p.Changes, true)
				c.publishLocked()
			}
			c.mu.Unlock()
		}

	case "account/updated":
		c.mu.Lock()
		c.resetPredictionLocked(c.prediction.turn)
		c.publishLocked()
		c.mu.Unlock()

	case "thread/reverted":
		var p struct {
			ThreadID string `json:"threadId"`
		}
		if json.Unmarshal(msg.Params, &p) == nil {
			c.mu.Lock()
			target := cmp.Or(p.ThreadID, c.mainThread)
			if target != "" && target == c.mainThread {
				c.resetPredictionLocked("")
				c.publishLocked()
			}
			c.mu.Unlock()
			if target != "" {
				go func() {
					c.reloadThreadTranscript(c.ctx, target)
					c.requestChanges(target)
				}()
			}
		}

	case "thread/started", "thread/status/changed":
		c.mu.Lock()
		mainID := c.mainThread
		c.mu.Unlock()
		if mainID != "" {
			go c.refreshSubtree(c.ctx, mainID)
		}

	case "thread/backgroundTerminals/changed":
		var p struct {
			ThreadID string `json:"threadId"`
		}
		if json.Unmarshal(msg.Params, &p) == nil && p.ThreadID != "" {
			c.mu.Lock()
			root := c.mainThread
			inFamily := p.ThreadID == root || c.family != nil && c.family.Root == root && slices.Contains(c.family.Threads, p.ThreadID)
			if inFamily {
				c.invalidateProcessListLocked(p.ThreadID)
			}
			c.mu.Unlock()
			if inFamily {
				go func() { _ = c.refreshProcesses(c.ctx, root) }()
			}
		}

	case "thread/closed":
		var p struct {
			ThreadID string `json:"threadId"`
		}
		if json.Unmarshal(msg.Params, &p) == nil && p.ThreadID != "" {
			c.mu.Lock()
			c.invalidateProcessListLocked(p.ThreadID)
			c.processes = slices.DeleteFunc(c.processes, func(process sessionpresentation.Process) bool {
				return process.Thread == p.ThreadID
			})
			c.updateProcessDetailsLocked()
			c.publishLocked()
			c.mu.Unlock()
		}

	case "serverRequest/resolved":
		if resolved, err := binding.DecodeResolvedRequest(msg.Params); err == nil {
			key := string(resolved.RequestID)
			c.mu.Lock()
			if _, exists := c.requests[key]; exists {
				delete(c.requests, key)
				c.requestOrder = slices.DeleteFunc(c.requestOrder, func(id string) bool { return id == key })
				c.publishLocked()
			}
			c.mu.Unlock()
		}
	}
}

func (c *directNativeConnection) handleServerRequest(msg appserver.Message) {
	key := string(msg.ID)
	req := &directPendingServerRequest{
		id:        slices.Clone(msg.ID),
		method:    msg.Method,
		rawParams: slices.Clone(msg.Params),
	}

	switch msg.Method {
	case "item/commandExecution/requestApproval", "item/fileChange/requestApproval", "item/permissions/requestApproval", "shellGuard/requestApproval":
		req.shellGuard = msg.Method == "shellGuard/requestApproval"
		view := decodeDirectApprovalView(key, msg.Method, msg.Params)
		req.threadID = view.Thread
		req.turnID = view.Turn
		req.itemID = view.Item
		req.approval = &view

	case "item/tool/requestUserInput":
		call := decodeDirectQuestionCall(key, msg.Params)
		req.threadID = call.Thread
		req.turnID = call.Turn
		req.itemID = call.Item
		req.question = &call

	case "shellGuard/requestSecret":
		req.secret = true
		req.shellGuard = true
		call := decodeDirectSecretCall(key, msg.Params)
		req.threadID = call.Thread
		req.question = &call

	default:
		_ = c.wire.RespondError(c.ctx, msg.ID, -32601, "unsupported terminal client server request: "+msg.Method)
		return
	}

	c.mu.Lock()
	if _, exists := c.requests[key]; !exists {
		c.requestOrder = append(c.requestOrder, key)
	}
	c.requests[key] = req
	c.publishLocked()
	c.mu.Unlock()
}

func decodeDirectApprovalView(id, method string, params jsontext.Value) prompt.ApprovalView {
	view := prompt.ApprovalView{
		ID: id,
		Choices: []prompt.Choice{
			{ID: 0, Label: "Allow", Approve: true},
			{ID: 1, Label: "Allow for session", Approve: true, Session: true, Lasting: true},
			{ID: 2, Label: "Deny", Deny: true},
		},
	}
	if decoded, err := binding.DecodeApprovalRequest(params); err == nil {
		view.Thread = decoded.ThreadID
		view.Turn = decoded.TurnID
		view.Item = decoded.ItemID
		view.Subject = decoded.Command
		if decoded.Reason != "" {
			view.Details = append(view.Details, decoded.Reason)
		}
		if decoded.GrantRoot != "" {
			view.Details = append(view.Details, "Grant root: "+decoded.GrantRoot)
		}
	}
	switch method {
	case "item/commandExecution/requestApproval":
		view.Title = "Approve command"
	case "item/fileChange/requestApproval":
		view.Title = "Approve file change"
	case "item/permissions/requestApproval":
		view.Title = "Approve permissions"
		var permEnv struct {
			Permissions jsontext.Value `json:"permissions"`
		}
		if json.Unmarshal(params, &permEnv) == nil && len(permEnv.Permissions) > 0 {
			var profile struct {
				FileSystem *struct {
					Read  []string `json:"read"`
					Write []string `json:"write"`
				} `json:"fileSystem"`
				Network *struct {
					Enabled bool `json:"enabled"`
				} `json:"network"`
			}
			added := false
			if json.Unmarshal(permEnv.Permissions, &profile) == nil {
				if fs := profile.FileSystem; fs != nil {
					if len(fs.Read) > 0 {
						view.Details = append(view.Details, "Read: "+strings.Join(fs.Read, ", "))
						added = true
					}
					if len(fs.Write) > 0 {
						view.Details = append(view.Details, "Write: "+strings.Join(fs.Write, ", "))
						added = true
					}
				}
				if net := profile.Network; net != nil && net.Enabled {
					view.Details = append(view.Details, "Network access: enabled")
					added = true
				}
			}
			if !added && string(permEnv.Permissions) != "{}" && string(permEnv.Permissions) != "null" {
				view.Details = append(view.Details, "Permissions: "+string(permEnv.Permissions))
			}
		}
	case "shellGuard/requestApproval":
		view.Title = "Approve guarded shell command"
		var sg struct {
			ThreadID   string   `json:"threadId"`
			TurnID     string   `json:"turnId"`
			ItemID     string   `json:"itemId"`
			Kind       string   `json:"kind"`
			Cwd        string   `json:"cwd"`
			Executable string   `json:"executable"`
			Argv       []string `json:"argv"`
			Command    string   `json:"command"`
			Summary    string   `json:"summary"`
			Reason     string   `json:"reason"`
		}
		if json.Unmarshal(params, &sg) == nil {
			if sg.ThreadID != "" {
				view.Thread = sg.ThreadID
			}
			if sg.TurnID != "" {
				view.Turn = sg.TurnID
			}
			if sg.ItemID != "" {
				view.Item = sg.ItemID
			}
			if cmd := formatShellGuardCommand(sg.Executable, sg.Argv); cmd != "" {
				view.Subject = cmd
			} else if sg.Command != "" {
				view.Subject = sg.Command
			}
			if sg.Kind != "" {
				view.Details = append(view.Details, "Kind: "+sg.Kind)
			}
			if sg.Cwd != "" {
				view.Details = append(view.Details, "Directory: "+sg.Cwd)
			}
			if sg.Summary != "" {
				view.Details = append(view.Details, sg.Summary)
			}
			if sg.Reason != "" {
				view.Details = append(view.Details, sg.Reason)
			}
		}
	}
	return view
}

func formatShellGuardCommand(executable string, argv []string) string {
	if executable == "" && len(argv) == 0 {
		return ""
	}
	parts := make([]string, 0, len(argv)+1)
	if executable != "" {
		if len(argv) > 0 && (argv[0] == executable || argv[0] == filepath.Base(executable)) {
			parts = append(parts, argv...)
		} else {
			parts = append(parts, executable)
			parts = append(parts, argv...)
		}
	} else {
		parts = append(parts, argv...)
	}
	return strings.Join(parts, " ")
}

func decodeDirectQuestionCall(id string, params jsontext.Value) prompt.CallView {
	call := prompt.CallView{ID: id, Sync: true}
	if decoded, err := binding.DecodeQuestionRequest(params); err == nil {
		call.Thread = decoded.ThreadID
		call.Turn = decoded.TurnID
		call.Item = decoded.ItemID
		for _, q := range decoded.Questions {
			qv := prompt.QuestionView{
				ID:     q.ID,
				Header: q.Header,
				Text:   q.Question,
				Secret: q.IsSecret,
			}
			for _, opt := range q.Options {
				qv.Choices = append(qv.Choices, prompt.Option{Label: opt.Label, Description: opt.Description})
			}
			call.Questions = append(call.Questions, qv)
		}
	}
	return call
}

func decodeDirectSecretCall(id string, params jsontext.Value) prompt.CallView {
	var sg struct {
		ThreadID   string   `json:"threadId"`
		TurnID     string   `json:"turnId"`
		ItemID     string   `json:"itemId"`
		Kind       string   `json:"kind"`
		Cwd        string   `json:"cwd"`
		Executable string   `json:"executable"`
		Argv       []string `json:"argv"`
		Prompt     string   `json:"prompt"`
		Command    string   `json:"command"`
	}
	_ = json.Unmarshal(params, &sg)
	text := sg.Prompt
	if text == "" {
		switch sg.Kind {
		case "sudoPassword":
			text = "Enter sudo password"
		case "gpgPassphrase":
			text = "Enter GPG passphrase"
		default:
			text = "Enter secret"
		}
	}
	cmd := formatShellGuardCommand(sg.Executable, sg.Argv)
	if cmd == "" {
		cmd = sg.Command
	}
	if cmd != "" {
		text += " (" + cmd + ")"
	}
	return prompt.CallView{
		ID:     id,
		Sync:   true,
		Thread: sg.ThreadID,
		Turn:   sg.TurnID,
		Item:   sg.ItemID,
		Questions: []prompt.QuestionView{{
			ID:     "secret",
			Header: "Secret required",
			Text:   text,
			Secret: true,
		}},
	}
}

func (c *directNativeConnection) findRecordBySeqLocked(threadID string, seq uint64) *directItemRecord {
	if seq == 0 {
		return nil
	}
	for _, rec := range c.items[threadID] {
		if rec.seq == seq {
			return rec
		}
	}
	return nil
}

func (c *directNativeConnection) ensureItemLocked(threadID, turnID, itemID, itemType, kind string) *directItemRecord {
	key := directItemKey{threadID, itemID}
	if itemID != "" {
		if rec, ok := c.itemsByID[key]; ok {
			return rec
		}
	}
	c.seq++
	agent := c.threadCallers[threadID]
	if threadID == "" || threadID == c.mainThread || agent == "" || agent == "/root" {
		if kind == "user" {
			agent = "You"
		} else {
			agent = "Main"
		}
	}
	rec := &directItemRecord{
		id:       itemID,
		threadID: threadID,
		turnID:   turnID,
		itemType: itemType,
		seq:      c.seq,
		entry: activity.Entry{
			Facts: activity.Facts{
				Seq:      c.seq,
				Agent:    agent,
				Kind:     kind,
				Observed: time.Now(),
			},
			Revision: 1,
		},
	}
	c.items[threadID] = append(c.items[threadID], rec)
	if itemID != "" {
		c.itemsByID[key] = rec
	}
	return rec
}

func (c *directNativeConnection) hydrateThreadTurnsLocked(threadID string, turns []directWireTurn) {
	delete(c.threadSkillReads, threadID)
	preservedSeq := make(map[string]uint64, len(c.items[threadID]))
	for _, rec := range c.items[threadID] {
		if rec.id != "" {
			preservedSeq[rec.id] = rec.seq
			delete(c.itemsByID, directItemKey{threadID, rec.id})
		}
	}
	c.items[threadID] = nil
	for _, turn := range turns {
		for _, rawItem := range turn.Items {
			c.upsertWireItemLocked(threadID, turn.ID, rawItem, true)
		}
		c.projectTurnErrorLocked(threadID, turn.ID, turn.Error)
	}
	for _, rec := range c.items[threadID] {
		rec.entry.Restored = true
		if prevSeq, ok := preservedSeq[rec.id]; ok {
			rec.seq = prevSeq
			rec.entry.Seq = prevSeq
			activity.Identify(rec.entry.Values, prevSeq)
		}
	}
}

// projectTurnErrorLocked shares one row for the live notification, completion,
// and the retained native turn. The full diagnostic stays in its detail value.
func directTurnErrorText(failure *binding.TurnError) string {
	if failure == nil || failure.Message == "" {
		return ""
	}
	diagnostic := failure.Message
	if failure.AdditionalDetails != "" {
		diagnostic += " · " + failure.AdditionalDetails
	}
	return diagnostic
}

func (c *directNativeConnection) clearRetryNoticeLocked(threadID, turnID string) bool {
	if len(c.retryNotices) == 0 {
		return false
	}
	if threadID == "" {
		clear(c.retryNotices)
		return true
	}
	notice, ok := c.retryNotices[threadID]
	if !ok || notice.text == "" {
		return false
	}
	if turnID != "" && notice.turnID != "" && notice.turnID != turnID {
		return false
	}
	delete(c.retryNotices, threadID)
	return true
}

func directItemIsResponseProgress(raw jsontext.Value, complete bool) bool {
	var item struct {
		Type   string `json:"type"`
		Source string `json:"source"`
	}
	if json.Unmarshal(raw, &item) != nil {
		return false
	}
	if complete {
		return item.Type == "agentMessage" || item.Type == "reasoning"
	}
	switch item.Type {
	case "agentMessage", "reasoning", "webSearch", "fileChange", "imageView", "imageGeneration",
		"mcpToolCall", "collabAgentToolCall", "dynamicToolCall":
		return true
	case "commandExecution":
		return item.Source != "userShell"
	}
	return false
}

func (c *directNativeConnection) hasPendingRequestForThreadLocked(threadID string) bool {
	if threadID == "" {
		return false
	}
	for _, req := range c.requests {
		if req != nil && req.threadID == threadID {
			return true
		}
	}
	return false
}

func (c *directNativeConnection) projectTurnErrorLocked(threadID, turnID string, failure *binding.TurnError) string {
	diagnostic := directTurnErrorText(failure)
	if diagnostic == "" || threadID == "" || turnID == "" {
		return ""
	}
	rec := c.ensureItemLocked(threadID, turnID, "native:turn-error:"+threadID+":"+turnID, "error", "error")
	if rec.entry.Completed.IsZero() {
		rec.entry.Completed = rec.entry.Observed
		rec.entry.ItemComplete = true
	}
	projectDirectError(rec, diagnostic)
	rec.entry.Revision++
	return diagnostic
}

func projectDirectError(rec *directItemRecord, diagnostic string) {
	rec.entry.Text = activity.ErrorPreview(diagnostic)
	rec.entry.Values = (activity.ItemContent{Facts: rec.entry.Facts, ErrorDetail: diagnostic}).Values()
}

// upsertWireItemLocked maps native mekugi-app-server ThreadItem payloads
// directly into activity.Entry publications. Unknown / server-owned inline card
// item payloads render directly from their server fields without Go backend state.
func (c *directNativeConnection) upsertWireItemLocked(threadID, turnID string, raw jsontext.Value, complete bool) string {
	var fields map[string]jsontext.Value
	if err := json.Unmarshal(raw, &fields); err != nil {
		return ""
	}
	itemType := decodeJSONString(fields["type"])
	itemID := decodeJSONString(fields["id"])
	if threadID != "" && threadID == c.btwThread {
		if itemType == "agentMessage" {
			if text := decodeJSONString(fields["text"]); text != "" {
				c.btw.Append(itemID, text, complete)
			}
		}
		return itemType
	}

	kind := "tool"
	switch itemType {
	case "userMessage":
		kind = "user"
		if clientID := decodeJSONString(fields["clientId"]); clientID != "" {
			c.commitSteerLocked(directSteerTurnID{thread: threadID, turn: turnID}, clientID)
		}
	case "agentMessage":
		if threadID == "" || threadID == c.mainThread {
			kind = "text"
		} else {
			kind = "final"
		}
	case "agentMail":
		switch decodeJSONString(fields["kind"]) {
		case "message":
			kind = "reply"
		case "reply":
			kind = "final"
		default:
			return itemType
		}
	case "reasoning":
		kind = "reasoning"
	case "plan":
		kind = "plan"
	case "contextCompaction":
		kind = "compaction"
	case "functionCallOutput":
		if decodeJSONString(fields["namespace"]) == "functions" &&
			decodeJSONString(fields["name"]) == "exec" && strings.HasPrefix(itemID, "code-mode-error:") {
			kind = "error"
			// A provider call ID is scoped to a turn, and forks may retain it.
			itemID = "native:tool-error:" + threadID + ":" + turnID + ":" + itemID
		}
	}

	rec := c.ensureItemLocked(threadID, turnID, itemID, itemType, kind)
	rec.entry.Kind = kind
	rec.entry.Live = !complete
	rec.entry.Running = !complete
	rec.entry.ItemComplete = complete
	if complete {
		rec.entry.Completed = time.Now()
		rec.entry.OutputComplete = true
	}

	if kind == "error" {
		diagnostic := "exec script failed"
		if errorText := decodeJSONString(fields["output"]); strings.TrimSpace(errorText) != "" {
			diagnostic += ": " + errorText
		}
		projectDirectError(rec, diagnostic)
		rec.entry.Revision++
		return itemType
	}

	switch itemType {
	case "userMessage":
		if first, ok := c.firstInputs[threadID]; ok && first.clientID == decodeJSONString(fields["clientId"]) {
			rec.seq, rec.entry.Seq = first.entry.Seq, first.entry.Seq
			rec.entry.Observed = first.entry.Observed
			// Replace the first displayed row, even if pre-turn work arrived first.
			records := c.items[threadID]
			index := slices.Index(records, rec)
			copy(records[1:index+1], records[:index])
			records[0] = rec
			delete(c.firstInputs, threadID)
		}
		message, spans := extractUserMessage(fields["content"])
		rec.entry.Text = c.observeQuestionReplyLocked(threadID, message)
		rec.entry.Spans = nil
		if rec.entry.Text == message {
			rec.entry.Spans = spans
		}
	case "agentMessage":
		if complete && decodeJSONString(fields["delivery"]) == "async" {
			c.observeAsyncQuestionsLocked(threadID, turnID, itemID, fields["questions"])
		}
		if text := decodeJSONString(fields["text"]); text != "" {
			rec.entry.Text = text
		}
		refreshProjectedValues(rec)
	case "agentMail":
		sender := decodeJSONString(fields["sender"])
		rec.entry.Agent = sender
		rec.entry.Delivered = true
		rec.entry.Text = decodeJSONString(fields["text"])
		valueKind := "message"
		if kind == "final" {
			valueKind = "final"
		}
		rec.entry.Values = []activity.Value{{
			Content: activityfeed.Content{
				Kind: valueKind,
				Body: textsanitize.MarkdownSource(rec.entry.Text),
			},
			From:         sender,
			To:           decodeJSONString(fields["recipient"]),
			Owner:        sender,
			ReplySummary: textsanitize.MarkdownSource(decodeJSONString(fields["summary"])),
			ReplyDetails: textsanitize.MarkdownSource(decodeJSONString(fields["details"])),
		}}
		activity.Identify(rec.entry.Values, rec.entry.Seq)
	case "plan":
		rec.entry.Text = decodeJSONString(fields["text"])
		rec.entry.Values = []activity.Value{{
			Content: activityfeed.Content{
				Kind: "message",
				Verb: "Plan",
				Body: rec.entry.Text,
			},
		}}
		activity.Identify(rec.entry.Values, rec.entry.Seq)
	case "reasoning":
		var summary []string
		_ = json.Unmarshal(fields["summary"], &summary)
		if len(summary) != 0 {
			rec.reasoningSummary = summary
		}
		refreshProjectedValues(rec)
		if complete {
			rec.reasoningSummary = nil
		}
	case "commandExecution":
		command := decodeJSONString(fields["command"])
		var display *struct {
			Script       string    `json:"script"`
			Segments     []string  `json:"segments"`
			StepKinds    []string  `json:"stepKinds"`
			StepSubjects []*string `json:"stepSubjects"`
			EffectiveCwd string    `json:"effectiveCwd"`
		}
		if raw := fields["display"]; len(raw) > 0 {
			_ = json.Unmarshal(raw, &display)
		}
		if display != nil {
			command = display.Script
		}
		status := decodeJSONString(fields["status"])
		cwd := decodeJSONString(fields["cwd"])
		agg := decodeJSONString(fields["aggregatedOutput"])
		if agg != "" {
			rec.output = agg
		}
		running := status == "inProgress"
		var exitCode *int
		if raw, ok := fields["exitCode"]; ok && len(raw) > 0 {
			_ = json.Unmarshal(raw, &exitCode)
		}
		var durationMs int64
		if raw, ok := fields["durationMs"]; ok && len(raw) > 0 {
			_ = json.Unmarshal(raw, &durationMs)
		}
		tail, tailOmitted := directCommandTail(rec.output)
		val := activity.Value{
			Content: activityfeed.Content{
				Kind:   "op",
				Verb:   "Run",
				Code:   command,
				Lang:   "bash",
				Fenced: strings.Contains(command, "\n"),
			},
			Workdir:     cwd,
			Running:     running,
			Exited:      exitCode != nil,
			Requested:   status == "declined",
			Failed:      status == "failed" || status == "declined",
			Duration:    time.Duration(durationMs) * time.Millisecond,
			Tail:        tail,
			TailOmitted: tailOmitted,
		}
		if exitCode != nil {
			val.ExitCode = *exitCode
		}
		if complete && status == "completed" && exitCode != nil && *exitCode == 0 {
			if names := activity.SkillReadNames(command); len(names) != 0 {
				if c.threadSkillReads == nil {
					c.threadSkillReads = make(map[string]activity.SkillSet)
				}
				c.threadSkillReads[threadID] = c.threadSkillReads[threadID].With(names...)
			}
		}
		if status == "declined" {
			val.Approval = "Denied"
		}
		if rec.output != "" {
			val.Output = 1
			if rec.entry.Outputs == nil {
				rec.entry.Outputs = map[int]int{}
			}
			rec.entry.Outputs[1]++
		}
		if display != nil && display.EffectiveCwd != "" {
			val.Workdir = display.EffectiveCwd
		}
		if filepath.Clean(val.Workdir) == filepath.Clean(c.workspace) {
			val.Workdir = ""
		}
		val.ShowWorkdir = val.Workdir != ""
		var actions []binding.CommandAction
		if display == nil || len(display.StepKinds) == 0 {
			_ = json.Unmarshal(fields["commandActions"], &actions)
		}
		var segments, kinds []string
		if display != nil {
			segments, kinds = display.Segments, display.StepKinds
		} else if len(actions) > 1 {
			for _, action := range actions {
				segments = append(segments, action.Command)
				kinds = append(kinds, action.Type)
			}
		}
		if len(segments) < 2 {
			if len(kinds) == 1 {
				val.Verb = directCommandVerb(kinds[0])
			} else if len(actions) == 1 {
				val.Verb = directCommandVerb(actions[0].Type)
			}
		}
		rec.entry.Values = nil
		if display != nil && len(display.StepSubjects) > 0 && len(segments) < 2 && display.StepSubjects[0] != nil {
			val.Label, val.DetailCode, val.Code = activity.SourceCode(*display.StepSubjects[0]), command, ""
			val.Fenced = false
		}
		if len(segments) == 1 {
			if val.Label == "" {
				val.Code = segments[0]
				if val.Code != command {
					val.DetailCode = command
				}
				val.Fenced = strings.Contains(val.Code, "\n")
			}
		} else if len(segments) > 1 {
			for i, source := range segments {
				verb := "Run"
				if i < len(kinds) {
					verb = directCommandVerb(kinds[i])
				}
				step := activity.Value{
					Content: activityfeed.Content{Kind: "op", Verb: verb, Code: source, Lang: "bash", Fenced: strings.Contains(source, "\n")},
					Workdir: val.Workdir, ShowWorkdir: i == 0 && val.ShowWorkdir, Operation: i + 2,
					Running: running, Requested: val.Requested,
				}
				if display != nil && i < len(display.StepSubjects) && display.StepSubjects[i] != nil {
					step.Label, step.DetailCode, step.Code = activity.SourceCode(*display.StepSubjects[i]), source, ""
					step.Fenced = false
				}
				rec.entry.Values = append(rec.entry.Values, step)
			}
			val.BatchExit, val.Operation, val.Label, val.DetailCode, val.Code = true, 1, "combined output", command, ""
			val.ShowWorkdir = false
		}
		rec.entry.Values = append(rec.entry.Values, val)
		activity.Identify(rec.entry.Values, rec.entry.Seq)
		rec.entry.Outcome = status
		rec.entry.Live = running
		rec.entry.Running = running
		rec.entry.OutputComplete = status != "" && !running
		if running {
			rec.entry.Completed = time.Time{}
		}
	case "imageView":
		path := decodeJSONString(fields["path"])
		rec.entry.Values = []activity.Value{{
			Path:    path,
			Running: !complete,
			Exited:  complete,
			Content: activityfeed.Content{
				Kind: "op",
				Verb: "View",
			},
		}}
		if path != "" {
			rec.entry.Images = []string{path}
		}
		activity.Identify(rec.entry.Values, rec.entry.Seq)

	case "fileChange":
		var changeList []nativeFileChange
		_ = json.Unmarshal(fields["changes"], &changeList)
		status := decodeJSONString(fields["status"])
		running := status == "inProgress"
		exited := status != "" && !running
		vals := make([]activity.Value, 0, len(changeList))
		for _, ch := range changeList {
			verb, path := ch.Kind.operation(ch.Path)
			label := activity.SourceCode(path)
			diff := ch.reviewFile()
			if status == "completed" {
				added, removed := diff.LineCounts()
				label += fmt.Sprintf(" +%d -%d", added, removed)
			}
			editOutcome := ""
			switch status {
			case "inProgress":
				editOutcome = "pending"
			case "failed", "declined":
				editOutcome = status
			}
			vals = append(vals, activity.Value{
				Path:        ch.Path,
				Effect:      true,
				EditOutcome: editOutcome,
				Content: activityfeed.Content{
					Kind:   "op",
					Verb:   verb,
					Label:  label,
					Code:   diff.Diff,
					Lang:   "diff",
					Fenced: diff.Diff != "",
				},
				Running: running,
				Exited:  exited,
			})
		}
		rec.entry.Values = vals
		activity.Identify(rec.entry.Values, rec.entry.Seq)
		rec.entry.Outcome = status
		rec.entry.OutputComplete = exited
		if exited && rec.entry.Completed.IsZero() {
			rec.entry.Completed = time.Now()
		}
		if running && len(changeList) > 0 {
			c.setNativeFilePreviewLocked(threadID, rec.turnID, rec.id, changeList, false)
		} else if exited {
			c.setNativeFilePreviewLocked(threadID, rec.turnID, rec.id, nil, false)
		}
	case "mcpToolCall", "dynamicToolCall":
		tool := decodeJSONString(fields["tool"])
		server := decodeJSONString(fields["server"])
		label := tool
		if server != "" && tool != "" {
			label = server + "/" + tool
		}
		rec.entry.Values = []activity.Value{{
			Content: activityfeed.Content{
				Kind:  "op",
				Verb:  "Call",
				Label: label,
			},
		}}
		activity.Identify(rec.entry.Values, rec.entry.Seq)
		rec.entry.Outcome = decodeJSONString(fields["status"])
	case "collabAgentToolCall":
		tool := decodeJSONString(fields["tool"])
		promptText := decodeJSONString(fields["prompt"])
		rec.entry.Values = []activity.Value{{
			Content: activityfeed.Content{
				Kind:  "op",
				Verb:  tool,
				Label: promptText,
			},
		}}
		activity.Identify(rec.entry.Values, rec.entry.Seq)
		rec.entry.Outcome = decodeJSONString(fields["status"])
	case "subAgentActivity":
		agentPath := decodeJSONString(fields["agentPath"])
		assignment := decodeJSONString(fields["assignment"])
		rec.entry.Assignment = assignment != ""
		rec.entry.AssignmentTo = agentPath
		rec.entry.AssignmentText = assignment
		rec.entry.Text = assignment
		rec.entry.Values = []activity.Value{{
			To: agentPath,
			Content: activityfeed.Content{
				Kind: "message",
				Body: assignment,
			},
		}}
		activity.Identify(rec.entry.Values, rec.entry.Seq)
	case "contextCompaction":
		if complete {
			delete(c.threadSkillReads, threadID)
		}
		rec.entry.ContextBoundary = true
		rec.entry.Text = "Context compacted"
		refreshProjectedValues(rec)
	default:
		// Render server-owned inline cards and future ThreadItem models directly
		// from their server-published fields without adding Go backend state.
		title := cmp.Or(
			decodeJSONString(fields["title"]),
			decodeJSONString(fields["header"]),
			decodeJSONString(fields["label"]),
			itemType,
		)
		body := cmp.Or(
			decodeJSONString(fields["body"]),
			decodeJSONString(fields["markdown"]),
			decodeJSONString(fields["text"]),
			decodeJSONString(fields["summary"]),
		)
		rec.entry.Text = body
		rec.entry.Values = []activity.Value{{
			Content: activityfeed.Content{
				Kind: "message",
				Verb: title,
				Body: body,
			},
		}}
		activity.Identify(rec.entry.Values, rec.entry.Seq)
		rec.entry.Outcome = decodeJSONString(fields["status"])
	}
	rec.entry.Revision++
	if itemType == "commandExecution" {
		if files := c.editReceipts[nativeEditItem{rec.threadID, rec.turnID, rec.id}]; len(files) > 0 {
			c.setEditReceiptLocked(rec, files)
		}
		c.refreshCommandDetailsLocked(rec)
		if processID := decodeJSONString(fields["processId"]); rec.entry.OutputComplete && processID != "" {
			c.removeLiveProcessLocked(sessionpresentation.Process{Thread: threadID, Turn: turnID, Item: itemID, ID: processID})
		} else {
			c.updateProcessDetailsLocked()
		}
	}
	return itemType
}

func refreshProjectedValues(rec *directItemRecord) {
	var details *activity.ItemDetails
	if rec.entry.Kind == "reasoning" {
		rec.entry.Text = strings.Join(rec.reasoningSummary, "\n\n")
		if strings.TrimSpace(rec.entry.Text) == "" {
			rec.entry.Values = nil
			return
		}
		details = &activity.ItemDetails{ReasoningLive: rec.entry.Live}
	}
	rec.entry.Values = (activity.ItemContent{
		Facts: rec.entry.Facts, Details: details, ReasoningParts: rec.reasoningSummary,
	}).Values()
}

func extractUserMessageText(raw jsontext.Value) string {
	message, _ := extractUserMessage(raw)
	return message
}

func extractUserMessage(raw jsontext.Value) (string, []activity.TextSpan) {
	var parts []struct {
		Type     string                     `json:"type"`
		Text     string                     `json:"text"`
		Name     string                     `json:"name"`
		Elements []binding.InputTextElement `json:"text_elements"`
	}
	if err := json.Unmarshal(raw, &parts); err != nil {
		return "", nil
	}
	var skillLabels map[string]bool
	var imagesRemaining int
	for _, part := range parts {
		if part.Type == "image" || part.Type == "localImage" {
			imagesRemaining++
		}
		if part.Type == "skill" && part.Name != "" {
			if skillLabels == nil {
				skillLabels = make(map[string]bool)
			}
			skillLabels["$"+part.Name] = true
		}
	}
	var message strings.Builder
	var spans []activity.TextSpan
	var imageIndex, markedImages int
	imageSpans := make(map[int]bool)
	var previousText bool
	for _, part := range parts {
		if part.Type == "image" || part.Type == "localImage" {
			imageIndex++
			imagesRemaining--
			// The browser sends its typed markers in text before the image parts.
			// The terminal sends only the image parts at their positions in text.
			if markedImages > 0 {
				markedImages--
				continue
			}
			if !previousText && message.Len() != 0 {
				message.WriteByte(' ')
			}
			start := message.Len()
			message.WriteString(fmt.Sprintf("[Image %d]", imageIndex))
			spans = append(spans, activity.TextSpan{Start: start, End: message.Len(), Kind: activity.ImageToken})
			previousText = false
			continue
		}
		if part.Text == "" {
			continue
		}
		if previousText {
			message.WriteByte('\n')
		}
		previousText = true
		offset := message.Len()
		message.WriteString(part.Text)
		for _, element := range part.Elements {
			start, end := element.ByteRange.Start, element.ByteRange.End
			if start < 0 || end <= start || end > len(part.Text) || part.Text[start:end] != element.Placeholder {
				continue
			}
			switch {
			case markedImages < imagesRemaining && !imageSpans[offset+start] &&
				strings.HasPrefix(element.Placeholder, "[Image ") && strings.HasSuffix(element.Placeholder, "]"):
				imageSpans[offset+start] = true
				markedImages++
				spans = append(spans, activity.TextSpan{Start: offset + start, End: offset + end, Kind: activity.ImageToken})
			case skillLabels[element.Placeholder]:
				spans = append(spans, activity.TextSpan{Start: offset + start, End: offset + end, Kind: activity.SkillToken})
			}
		}
	}
	slices.SortFunc(spans, func(a, b activity.TextSpan) int { return cmp.Compare(a.Start, b.Start) })
	previousEnd := 0
	spans = slices.DeleteFunc(spans, func(span activity.TextSpan) bool {
		if span.Start < previousEnd {
			return true
		}
		previousEnd = span.End
		return false
	})
	return message.String(), spans
}

func directCommandVerb(kind string) string {
	switch kind {
	case "read":
		return "Read"
	case "search":
		return "Search"
	case "list", "listFiles":
		return "List"
	case "inspect":
		return "Inspect"
	case "stage":
		return "Stage"
	case "commit":
		return "Commit"
	case "skill":
		return "Skill"
	default:
		return "Run"
	}
}

func decodeJSONString(raw jsontext.Value) string {
	if len(raw) == 0 {
		return ""
	}
	var s string
	if err := json.Unmarshal(raw, &s); err == nil {
		return s
	}
	return ""
}

type nativeFileChange struct {
	Path string          `json:"path"`
	Kind nativePatchKind `json:"kind"`
	Diff string          `json:"diff"`
}

type nativePatchKind struct {
	Type     string `json:"type"`
	MovePath string `json:"move_path"`
}

func (kind nativePatchKind) operation(path string) (verb, label string) {
	switch kind.Type {
	case "add":
		return "Create", path
	case "delete":
		return "Delete", path
	case "update":
		if kind.MovePath != "" {
			return "Move", path + " => " + kind.MovePath
		}
	}
	return "Edit", path
}

func (c *directNativeConnection) removeNativePreviewsLocked(match func(preview.Preview) bool) {
	if slices.ContainsFunc(c.live, match) {
		c.live = slices.DeleteFunc(c.live, match)
		c.liveDirty = true
	}
}

func (c *directNativeConnection) setNativeFilePreviewLocked(threadID, turnID, itemID string, updates []nativeFileChange, unlocatedDiff bool) {
	id := "filechange:" + itemID
	match := func(p preview.Preview) bool { return p.Thread == threadID && p.Turn == turnID && p.ID == id }
	if len(updates) == 0 {
		c.removeNativePreviewsLocked(match)
		return
	}
	files := make([]diffmodel.ReviewFile, 0, len(updates))
	for _, update := range updates {
		files = append(files, update.reviewFile())
	}
	edit := preview.Preview{
		ID: id, Workspace: c.workspace, Caller: cmp.Or(c.threadCallers[threadID], "/root"),
		Turn: turnID, Thread: threadID, Files: files, DiffText: true, UnlocatedDiff: unlocatedDiff, Status: "Editing",
	}
	if index := slices.IndexFunc(c.live, match); index >= 0 {
		c.live[index] = edit
	} else {
		c.live = append(c.live, edit)
	}
	c.liveDirty = true
}

func (change nativeFileChange) reviewFile() diffmodel.ReviewFile {
	file := diffmodel.ReviewFile{
		BeforePath: change.Path,
		AfterPath:  cmp.Or(change.Kind.MovePath, change.Path),
		Diff:       change.Diff,
	}
	switch change.Kind.Type {
	case "add":
		file.BeforePath = ""
		file.Diff = nativePreviewWholeFileDiff(file.Diff, '+')
	case "delete":
		file.AfterPath = ""
		file.Diff = nativePreviewWholeFileDiff(file.Diff, '-')
	}
	return file
}

// Native adds and deletes carry raw file content, not a unified patch.
func nativePreviewWholeFileDiff(content string, kind byte) string {
	if content == "" {
		return ""
	}
	rows := strings.Count(content, "\n")
	terminated := strings.HasSuffix(content, "\n")
	if !terminated {
		rows++
	}
	var diff strings.Builder
	if kind == '+' {
		fmt.Fprintf(&diff, "@@ -0,0 +1,%d @@\n", rows)
	} else {
		fmt.Fprintf(&diff, "@@ -1,%d +0,0 @@\n", rows)
	}
	for line := range strings.SplitAfterSeq(content, "\n") {
		if line != "" {
			diff.WriteByte(kind)
			diff.WriteString(line)
		}
	}
	if !terminated {
		diff.WriteString("\n\\ No newline at end of file\n")
	}
	return diff.String()
}

func (c *directNativeConnection) observeThreadSkillsLocked(threadID string, names []string) {
	if threadID == "" || len(names) == 0 {
		return
	}
	if c.threadSkills == nil {
		c.threadSkills = make(map[string]activity.SkillSet)
	}
	c.threadSkills[threadID] = c.threadSkills[threadID].With(names...)
}

func (c *directNativeConnection) loadedSkillsLocked(threadID string) activity.SkillSet {
	return slices.Clone(c.threadSkills[threadID]).With(c.threadSkillReads[threadID]...)
}

func (c *directNativeConnection) refreshSubtree(ctx context.Context, rootThread string) {
	var resp struct {
		Data []directWireThread `json:"data"`
	}
	if err := c.wire.Call(ctx, "thread/subtree/list", map[string]any{"threadId": rootThread}, &resp); err != nil {
		return
	}
	c.mu.Lock()
	if c.mainThread != rootThread {
		c.mu.Unlock()
		return
	}
	var threads []string
	var roster []activity.AgentRow
	for i, th := range resp.Data {
		if _, found := c.threadUsage[th.ID]; !found {
			c.applyThreadUsageLocked(th.ID, th.TokenUsage)
		}
		c.observeThreadSkillsLocked(th.ID, th.Extra.Skills)
		threads = append(threads, th.ID)
		var name string
		if th.ID == rootThread {
			name = "/root"
		} else {
			parentID := cmp.Or(th.ParentThreadID, rootThread)
			name = (binding.ThreadInfo{ParentThreadID: parentID, Source: th.Source}).AgentPath()
			if name == "" && th.AgentNickname != "" {
				name = "/root/" + th.AgentNickname
			}
			if name == "" {
				name = fmt.Sprintf("/root/agent-%d", i)
			}
		}
		c.threadCallers[th.ID] = name
		depth := strings.Count(strings.TrimPrefix(name, "/"), "/")
		if th.ID == rootThread {
			depth = 0
		}
		var statusObj struct {
			Type        string   `json:"type"`
			ActiveFlags []string `json:"activeFlags"`
		}
		_ = json.Unmarshal(th.Status, &statusObj)
		agentState := (binding.ThreadInfo{AgentStatus: th.AgentStatus}).AgentState()
		paused := agentState == "interrupted"
		waiting := statusObj.Type == "active" &&
			(slices.Contains(statusObj.ActiveFlags, "waitingOnApproval") ||
				slices.Contains(statusObj.ActiveFlags, "waitingOnUserInput"))
		responding := agentState == "running" || agentState == "" && statusObj.Type == "active"
		final := agentState == "completed" && th.ID != rootThread
		latestKind := ""
		if agentState == "errored" || agentState == "" && statusObj.Type == "systemError" {
			latestKind = "error"
		}
		facts := activity.AgentFacts{
			Name:       name,
			Role:       th.AgentRole,
			Thread:     th.ID,
			Responding: responding,
			Waiting:    waiting,
			Paused:     paused,
			Final:      final,
		}
		roster = append(roster, activity.AgentRow{
			Agent: activity.Agent{
				Status:     facts.ObservedStatus(latestKind),
				LatestKind: latestKind,
				AgentFacts: facts,
			},
			Depth: depth,
		})
	}
	processScopeChanged := c.family == nil || !slices.Equal(c.family.Threads, threads)
	c.family = &sessionapi.Family{
		Root:    rootThread,
		Threads: threads,
		Agents:  &activity.Publication{Roster: roster},
		Status:  sessionpresentation.Label{Text: c.statusTextLocked(), Running: c.busy},
	}
	c.publishLocked()
	c.mu.Unlock()
	if processScopeChanged {
		_ = c.refreshProcesses(ctx, rootThread)
	}
}

type directRateLimitWindow struct {
	Used    float64 `json:"usedPercent"`
	Minutes *int64  `json:"windowDurationMins"`
	Resets  *int64  `json:"resetsAt"`
}

func (w directRateLimitWindow) label(position int) string {
	if w.Minutes == nil {
		return []string{"primary", "secondary"}[position]
	}
	switch minutes := *w.Minutes; {
	case minutes == 10080:
		return "weekly"
	case minutes > 0 && minutes%60 == 0:
		return fmt.Sprintf("%dh", minutes/60)
	default:
		return fmt.Sprintf("%dm", minutes)
	}
}

type directRateLimitSnapshot struct {
	ID        *string                `json:"limitId"`
	Name      string                 `json:"limitName"`
	Primary   *directRateLimitWindow `json:"primary"`
	Secondary *directRateLimitWindow `json:"secondary"`
	Credits   *struct {
		Unlimited bool    `json:"unlimited"`
		Balance   *string `json:"balance"`
	} `json:"credits"`
}

func directObserveRateLimitSnapshot(windows *[2]*sessionpresentation.CaptionLimit, snapshot directRateLimitSnapshot) {
	if windows == nil {
		return
	}
	for i, w := range []*directRateLimitWindow{snapshot.Primary, snapshot.Secondary} {
		if w != nil {
			windows[i] = &sessionpresentation.CaptionLimit{
				Label: w.label(i),
				Used:  max(0, min(100, w.Used)),
			}
		}
	}
}

func directObserveRateLimits(windows *[2]*sessionpresentation.CaptionLimit, params []byte) error {
	var update struct {
		RateLimits directRateLimitSnapshot `json:"rateLimits"`
	}
	if err := json.Unmarshal(params, &update); err != nil {
		return err
	}
	if id := update.RateLimits.ID; id != nil && *id != "" && *id != "codex" {
		return nil
	}
	directObserveRateLimitSnapshot(windows, update.RateLimits)
	return nil
}

func directCaptionLimits(windows [2]*sessionpresentation.CaptionLimit) []sessionpresentation.CaptionLimit {
	var limits []sessionpresentation.CaptionLimit
	for _, w := range windows {
		if w != nil {
			limits = append(limits, *w)
		}
	}
	return limits
}

type directRateLimitsResponse struct {
	RateLimits directRateLimitSnapshot            `json:"rateLimits"`
	ByID       map[string]directRateLimitSnapshot `json:"rateLimitsByLimitId"`
	Resets     *struct {
		Available int64 `json:"availableCount"`
		Credits   []struct {
			Type    string `json:"resetType"`
			Status  string `json:"status"`
			Expires *int64 `json:"expiresAt"`
			Title   string `json:"title"`
		} `json:"credits"`
	} `json:"rateLimitResetCredits"`
}

func (c *directNativeConnection) refreshRateLimits(ctx context.Context) (*directRateLimitsResponse, error) {
	if c.wire == nil {
		return nil, nil
	}
	var resp directRateLimitsResponse
	if err := c.wire.Call(ctx, "account/rateLimits/read", nil, &resp); err != nil {
		return nil, err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if codex, ok := resp.ByID["codex"]; ok {
		directObserveRateLimitSnapshot(&c.rateLimits, codex)
	} else {
		directObserveRateLimitSnapshot(&c.rateLimits, resp.RateLimits)
	}
	c.publishLocked()
	return &resp, nil
}

func (c *directNativeConnection) reloadThreadTranscript(ctx context.Context, threadID string) {
	c.mu.Lock()
	waitingForFirstInput := len(c.items[threadID]) == 0 || c.firstInputs[threadID].clientID != ""
	c.mu.Unlock()
	var resp struct {
		Thread directWireThread `json:"thread"`
	}
	if err := c.wire.Call(ctx, "thread/read", map[string]any{"threadId": threadID, "includeTurns": true}, &resp); err != nil {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	records := c.items[threadID]
	preserveFirstInput := waitingForFirstInput && len(records) > 0 &&
		(threadID == c.mainThread || threadID == c.sideThread) && records[0].entry.Kind == "user"
	if _, hasUsage := c.threadUsage[threadID]; !preserveFirstInput || !hasUsage {
		c.applyThreadUsageLocked(threadID, resp.Thread.TokenUsage)
	}
	c.observeThreadSkillsLocked(threadID, resp.Thread.Extra.Skills)
	if preserveFirstInput {
		// The first canonical user item arrived during this read. Do not replace
		// the live initial transcript with a snapshot that may predate it.
		c.publishLocked()
		return
	}
	turns := resp.Thread.Turns
	if threadID == c.sideThread {
		first := slices.IndexFunc(turns, func(turn directWireTurn) bool { return turn.ID == c.sideFirstTurn })
		if c.sideFirstTurn == "" || first < 0 {
			return
		}
		turns = turns[first:]
	}
	c.hydrateThreadTurnsLocked(threadID, turns)
	c.publishLocked()
}

type directResumePage struct {
	Data       []directWireThread `json:"data"`
	NextCursor string             `json:"nextCursor"`
}

func resumeThreadLabel(th directWireThread) string {
	return cmp.Or(strings.TrimSpace(th.Name), strings.TrimSpace(th.Preview))
}

func (c *directNativeConnection) openResumePicker(ctx context.Context) error {
	c.mu.Lock()
	previous := c.resumeView
	view := sessionpresentation.ResumeView{Workspace: c.workspace, Startup: c.mainThread == ""}
	c.mu.Unlock()
	return c.loadResumeList(ctx, previous, view)
}

func (c *directNativeConnection) loadResumeList(ctx context.Context, previous *sessionpresentation.ResumeView, view sessionpresentation.ResumeView) error {
	params := map[string]any{
		"limit": 50, "modelProviders": []string{}, "archived": view.Archived,
		"currentAccountOnly": true, "sortKey": "updated_at",
	}
	cursor := view.PageAfter
	if cursor != "" {
		params["cursor"] = cursor
	}
	if !view.All && view.Workspace != "" {
		params["cwd"] = view.Workspace
	}
	var resp directResumePage
	if err := c.wire.Call(ctx, "thread/list", params, &resp); err != nil {
		return c.reportResumeError(previous, fmt.Errorf("list native threads: %w", err), cursor != "")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.resumeView != previous {
		return nil
	}
	if view.ID == 0 {
		view.ID = c.revision + 1
	}
	view.Rows = slices.Clone(view.Rows)
	for _, th := range resp.Data {
		if th.ID == "" || th.ThreadSource == "mekugi_side" {
			continue
		}
		title, preview := resumeThreadLabel(th), strings.TrimSpace(th.Preview)
		if title == preview {
			preview = ""
		}
		view.Rows = append(view.Rows, sessionpresentation.ResumeRow{
			ID:        th.ID,
			Title:     title,
			Preview:   preview,
			Workspace: th.Cwd,
			Updated:   th.UpdatedAt,
			Branch:    th.GitInfo.Branch,
		})
	}
	view.PageAfter, view.Problem = resp.NextCursor, ""
	if cursor == "" {
		view.LoadedAt = time.Now()
	}
	c.resumeView = &view
	c.publishLocked()
	return nil
}

func (c *directNativeConnection) reportResumeError(view *sessionpresentation.ResumeView, err error, stopPaging bool) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if view != nil && c.resumeView == view {
		next := *view
		next.Problem = err.Error()
		if stopPaging {
			next.PageAfter = ""
		}
		c.resumeView = &next
		c.publishLocked()
	}
	return err
}

func (c *directNativeConnection) setFeedbackLocked(text string, alert bool) {
	c.feedbackText = text
	c.feedbackAlert = alert
	c.feedbackRev++
}
