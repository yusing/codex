package launch

import (
	"bytes"
	"cmp"
	"context"
	"errors"
	"fmt"
	"image"
	"io"
	"maps"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionapi"
	"github.com/mekugi-harness/mekugi/mekugi-tui/apps/mekugi/internal/sessionpresentation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/backends/codex/appserver"
	"github.com/mekugi-harness/mekugi/mekugi-tui/contracts/observation"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/pathdisplay"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/sourceanalysis/sourcekind"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/sourceanalysis/sourceparse"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/terminalui"
	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/textsanitize"
	activityfeed "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/feed"
	activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"
	activityui "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/terminalview"
	"github.com/mekugi-harness/mekugi/mekugi-tui/modules/conversation/completion"
)

func clonePanelView(p *sessionpresentation.PanelView) *sessionpresentation.PanelView {
	if p == nil {
		return nil
	}
	cp := *p
	cp.Fields = slices.Clone(p.Fields)
	cp.FeatureFields = slices.Clone(p.FeatureFields)
	return &cp
}

func (c *directNativeConnection) handleOpenDetails(ctx context.Context, req sessionapi.NativeDetailRequest) error {
	switch req.Kind {
	case sessionapi.NativeFileDetails:
		return c.openFileDetails(ctx, req)
	case sessionapi.NativeSkillDetails:
		return c.openSkillDetails(ctx, req)
	case sessionapi.NativeProcessDetails:
		return c.openProcessDetails(ctx, req)
	case sessionapi.NativeErrorDetails:
		c.mu.Lock()
		defer c.mu.Unlock()
		c.openErrorDetailsLocked(req)
		return nil
	case sessionapi.NativeRunDetails:
		c.mu.Lock()
		defer c.mu.Unlock()
		return c.openEntryDetailsLocked(req, true)
	default:
		c.mu.Lock()
		defer c.mu.Unlock()
		return c.openEntryDetailsLocked(req, false)
	}
}

func (c *directNativeConnection) openEntryDetailsLocked(req sessionapi.NativeDetailRequest, combinedRun bool) error {
	threadID := req.Displayed.ID
	if threadID == "" {
		if req.Agent && c.displayedThread != "" {
			threadID = c.displayedThread
		} else {
			threadID = c.mainThread
		}
	}
	sources := req.Sources
	var runRange *sessionpresentation.RunOutputRange
	if combinedRun && req.Path != "" {
		id, first, last, ok := commandRunTarget(req.Path)
		if !ok {
			return errors.New("the command run link is invalid")
		}
		rec := c.itemsByID[directItemKey{threadID, id}]
		if rec == nil || rec.itemType != "commandExecution" {
			return errors.New("the command run is unavailable in this thread")
		}
		sources = []uint64{rec.seq}
		if first > 0 {
			runRange = &sessionpresentation.RunOutputRange{First: first, Last: last}
		}
	}
	if len(sources) == 0 && req.Source != 0 {
		sources = []uint64{req.Source}
	}
	var values []activity.Value
	for _, seq := range sources {
		rec := c.findRecordBySeqLocked(threadID, seq)
		if rec == nil {
			continue
		}
		values = append(values, c.entryDetailValuesLocked(rec, combinedRun)...)
	}
	if len(values) == 0 && req.Path != "" {
		if rec, ok := c.itemsByID[directItemKey{threadID, req.Path}]; ok {
			values = append(values, c.entryDetailValuesLocked(rec, combinedRun)...)
		}
	}
	if len(values) == 0 {
		return errors.New("detail target not found")
	}
	pages := activity.DetailPages(values)
	if len(pages) == 0 {
		return errors.New("no detail pages available")
	}
	items := make([]sessionpresentation.DetailItem, 0, len(pages))
	selected := pages[0].ID
	var selectedRec *directItemRecord
	var selectedPage sessionpresentation.DetailItem
	for i, page := range pages {
		page.CombinedOutput = combinedRun || page.BatchExit
		rec := c.findRecordBySeqLocked(threadID, page.Source)
		var outVer uint64
		retained := false
		outputLive := false
		if rec != nil {
			outVer = rec.entry.Revision
			if page.Output != 0 && rec.output != "" || len(page.Tail) > 0 {
				retained = true
			}
			outputLive = rec.entry.Running
		}
		item := sessionpresentation.DetailItem{
			DetailPage:    page,
			OutputVersion: outVer,
			Retained:      retained,
			OutputLive:    outputLive,
		}
		items = append(items, item)
		if i == 0 ||
			(req.Operation != 0 && page.Operation == req.Operation) ||
			(req.Output != 0 && page.Output == req.Output) ||
			(req.Path != "" && (page.Path == req.Path || page.Label == req.Path)) {
			selected = page.ID
			selectedRec = rec
			selectedPage = item
		}
	}
	if selectedRec != nil && selectedRec.itemType == "commandExecution" && !selectedPage.Effect {
		for _, item := range items {
			if item.Source == selectedRec.seq && item.BatchExit {
				selected, selectedPage = item.ID, item
				break
			}
		}
	}
	c.detailSeq++
	id := sessionpresentation.DetailID{Attachment: c.attachID, Selection: c.detailSeq}
	caller := c.threadCallers[threadID]
	if caller == "" {
		caller = "/root"
	}
	details := &sessionapi.NativeDetails{
		Source:   sessionpresentation.Thread{ID: threadID, Caller: caller},
		Selected: selected,
		Agent:    req.Agent,
		RunRange: runRange,
		View: sessionpresentation.DetailView{
			Revision: c.detailSeq,
			ID:       id,
			Pages:    items,
		},
	}
	if selectedPage.Output > 0 {
		c.setDetailsOutputLocked(details, selectedPage.ID, selectedPage.OutputVersion, selectedRec, selectedPage.DetailPage)
	}
	c.details = details
	c.publishLocked()
	return nil
}

func (c *directNativeConnection) entryDetailValuesLocked(rec *directItemRecord, combinedRun bool) []activity.Value {
	values := activity.CloneValues(rec.entry.Values)
	if combinedRun && len(values) > 1 && values[len(values)-1].BatchExit {
		values = values[len(values)-1:]
	}
	if !combinedRun && (rec.itemType == "commandExecution" || rec.itemType == "fileChange") {
		if edits := c.capturedDetailValuesLocked(rec); len(edits) > 0 {
			if rec.itemType == "fileChange" {
				for _, edit := range edits {
					matched := false
					for i := range values {
						path := values[i].Path
						if !filepath.IsAbs(path) {
							path = filepath.Join(edit.Workdir, path)
						}
						if path == edit.Path {
							edit.Operation = values[i].Operation
							values[i] = edit
							matched = true
							break
						}
					}
					if !matched {
						values = append(values, edit)
					}
				}
			} else {
				values = append(values, edits...)
			}
			activity.Identify(values, rec.seq)
		}
	}
	return values
}

func (c *directNativeConnection) openFileDetails(ctx context.Context, req sessionapi.NativeDetailRequest) error {
	c.mu.Lock()
	workspace := c.workspace
	threadID := cmp.Or(req.Displayed.ID, c.mainThread)
	caller := cmp.Or(c.threadCallers[threadID], "/root")
	attachment := c.attachID
	c.mu.Unlock()

	sel, ok := markdownFileTarget(workspace, req.Path)
	if !ok {
		return fmt.Errorf("invalid file path: %s", req.Path)
	}
	sel, _, err := sel.stat()
	var source string
	var raster bool
	if err == nil {
		source, raster, err = readDetailFile(ctx, sel.path)
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	displayPath := pathdisplay.ForWorkspace(workspace, sel.path)
	file := &activity.DetailFile{Path: displayPath, Source: source, First: sel.first, Last: sel.last}
	path := displayPath + sel.location
	value := activity.Value{Kind: "reads", Verb: "Read", Path: path, SyntaxPath: sel.path,
		Reads: []activityfeed.Read{{Path: path}},
	}
	var pages []activity.Value
	switch {
	case err != nil:
		value.Kind, value.Verb, value.Body = "error", "", err.Error()
	case raster:
		value.Verb, file.ImagePath = "View", sel.path
	default:
		content := textsanitize.Safe(source, false)
		value.Tail = strings.Split(content, "\n")
		if format, _ := sourcekind.Classify(sel.path); format.Kind == "markdown" {
			rendered := value
			rendered.Body, rendered.Tail, rendered.DetailLines = content, nil, len(value.Tail)
			pages = append(pages, rendered)
		}
	}
	pages = append(pages, value)
	items := make([]sessionpresentation.DetailItem, 0, len(pages))
	for _, page := range activity.DetailPages(pages) {
		items = append(items, sessionpresentation.DetailItem{DetailPage: page})
	}
	selected := items[0].ID
	if file.First > 0 {
		selected = items[len(items)-1].ID
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.attachID != attachment {
		return sessionpresentation.ErrStaleIntent
	}
	c.detailSeq++
	id := sessionpresentation.DetailID{Attachment: c.attachID, Selection: c.detailSeq}
	c.details = &sessionapi.NativeDetails{
		Source:   sessionpresentation.Thread{ID: threadID, Caller: caller},
		Selected: selected,
		Agent:    req.Agent,
		File:     file,
		View: sessionpresentation.DetailView{
			Revision: c.detailSeq,
			ID:       id,
			Pages:    items,
		},
	}
	c.publishLocked()
	return nil
}

// Source: mekugi-tui/apps/mekugi/internal/launch/markdown_file_dialog.go:283:335@[cc041cd0] readMarkdownFile
func readDetailFile(ctx context.Context, path string) (string, bool, error) {
	const limit = 8 << 20
	file, err := sourceparse.Open(path)
	if err != nil {
		return "", false, err
	}
	defer file.Close()
	stop := context.AfterFunc(ctx, func() { _ = file.Close() })
	defer stop()
	info, err := file.Stat()
	if err != nil {
		return "", false, err
	}
	if !info.Mode().IsRegular() {
		return "", false, errors.New("not a regular file")
	}
	data, err := io.ReadAll(io.LimitReader(file, limit+1))
	if err != nil {
		return "", false, err
	}
	if len(data) <= limit && utf8.Valid(data) && bytes.IndexByte(data, 0) < 0 {
		return string(data), false, nil
	}
	reader := io.MultiReader(bytes.NewReader(data), io.LimitReader(file, terminalui.ImageInputLimit-int64(len(data))))
	_, format, err := image.DecodeConfig(reader)
	if format != "" {
		if err != nil {
			return "", false, fmt.Errorf("image preview unavailable: %w", err)
		}
		if info.Size() > terminalui.ImageInputLimit {
			return "", false, fmt.Errorf("image exceeds %d-byte preview limit", terminalui.ImageInputLimit)
		}
		return "", true, nil
	}
	if len(data) > limit {
		return "", false, fmt.Errorf("file exceeds %d-byte dialog limit", limit)
	}
	return "", false, errors.New("file is not UTF-8 text")
}

func (c *directNativeConnection) openSkillDetails(ctx context.Context, req sessionapi.NativeDetailRequest) error {
	c.mu.Lock()
	cwd := c.workspace
	threadID := cmp.Or(req.Displayed.ID, c.displayedThread, c.mainThread)
	caller := cmp.Or(c.threadCallers[threadID], req.Displayed.Caller, "/root")
	names := c.loadedSkillsLocked(threadID)
	c.mu.Unlock()

	var b strings.Builder
	if len(names) == 0 {
		b.WriteString("No skills loaded.")
	} else {
		skills := c.loadSkills(ctx, cwd)
		for _, name := range names {
			fmt.Fprintf(&b, "- **%s**", name)
			index := slices.IndexFunc(skills, func(skill completion.Choice) bool { return skill.Name == name })
			if index < 0 {
				b.WriteByte('\n')
				continue
			}
			sk := skills[index]
			if sk.Description != "" {
				fmt.Fprintf(&b, " — %s", sk.Description)
			}
			if sk.Path != "" {
				fmt.Fprintf(&b, " (`%s`)", sk.Path)
			}
			b.WriteByte('\n')
		}
	}
	page := activity.DetailPage{
		ID: "skills",
		Value: activity.Value{
			Content: activityfeed.Content{
				Kind: "text",
				Verb: "Skills",
				Body: strings.TrimSpace(b.String()),
			},
		},
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.detailSeq++
	id := sessionpresentation.DetailID{Attachment: c.attachID, Selection: c.detailSeq}
	c.details = &sessionapi.NativeDetails{
		Source:   sessionpresentation.Thread{ID: threadID, Caller: caller},
		Selected: page.ID,
		Agent:    req.Agent,
		View: sessionpresentation.DetailView{
			Revision: c.detailSeq,
			ID:       id,
			Pages:    []sessionpresentation.DetailItem{{DetailPage: page}},
		},
	}
	c.publishLocked()
	return nil
}

func (c *directNativeConnection) openErrorDetailsLocked(req sessionapi.NativeDetailRequest) {
	threadID := cmp.Or(req.Displayed.ID, c.displayedThread, c.mainThread)
	var values []activity.Value
	for _, rec := range c.items[threadID] {
		if rec.entry.Kind == "error" && !rec.entry.Hidden() && (req.Source == 0 || req.Source == rec.seq) {
			values = append(values, rec.entry.Values...)
		}
	}
	c.publishErrorDetailsLocked(threadID, req.Agent, values)
}

func (c *directNativeConnection) publishErrorDetailsLocked(threadID string, agent bool, values []activity.Value) {
	pages := activity.DetailPages(values)
	if len(pages) == 0 {
		return
	}
	items := make([]sessionpresentation.DetailItem, 0, len(pages))
	for _, p := range pages {
		items = append(items, sessionpresentation.DetailItem{DetailPage: p})
	}
	c.detailSeq++
	id := sessionpresentation.DetailID{Attachment: c.attachID, Selection: c.detailSeq}
	caller := cmp.Or(c.threadCallers[threadID], "/root")
	c.details = &sessionapi.NativeDetails{
		Source:   sessionpresentation.Thread{ID: threadID, Caller: caller},
		Selected: items[len(items)-1].ID,
		Agent:    agent,
		View: sessionpresentation.DetailView{
			Revision: c.detailSeq,
			ID:       id,
			Pages:    items,
		},
	}
	c.publishLocked()
}

// Follow the command, not its enclosing item's or turn's completion event.
func (c *directNativeConnection) refreshCommandDetailsLocked(rec *directItemRecord) {
	if c.details == nil || c.details.View.Processes || len(rec.entry.Values) == 0 {
		return
	}
	result := rec.entry.Values[len(rec.entry.Values)-1]
	details := *c.details
	for i, page := range details.View.Pages {
		if page.Source != rec.seq || page.Operation != result.Operation {
			continue
		}
		details.View.Pages = slices.Clone(details.View.Pages)
		page.Value = result
		page.OutputVersion = rec.entry.Revision
		page.Retained = rec.output != ""
		page.OutputLive = rec.entry.Running
		details.View.Pages[i] = page
		if page.Output > 0 && (details.Selected == page.ID ||
			details.Output != nil && details.Output.Page == page.ID) {
			c.setDetailsOutputLocked(&details, page.ID, page.OutputVersion, rec, page.DetailPage)
		}
		details.View.Revision++
		c.details = &details
		return
	}
}

func (c *directNativeConnection) setDetailsOutputLocked(details *sessionapi.NativeDetails, pageID string, version uint64, rec *directItemRecord, page activity.DetailPage) {
	details.Output = c.buildDetailOutputLocked(details.View.ID, pageID, version, rec, page)
	if details.RunRange == nil {
		return
	}
	selected := *details.RunRange
	lines := details.Output.View.Lines
	limit := len(lines)
	for i, line := range lines {
		if count, ok := strings.CutPrefix(line, "... "); ok && strings.HasSuffix(count, " bytes omitted ...") {
			// The prefix's last line may have been clipped mid-line.
			limit = max(0, i-1)
			break
		}
	}
	selected.Available = selected.Last <= limit
	details.RunRange = &selected
}

func (c *directNativeConnection) buildDetailOutputLocked(id sessionpresentation.DetailID, pageID string, version uint64, rec *directItemRecord, page activity.DetailPage) *sessionpresentation.DetailOutput {
	var lines []string
	done := !page.Running
	if rec != nil {
		lines = directOutputLines(rec.output)
	}
	if len(lines) == 0 && len(page.Tail) > 0 {
		lines = slices.Clone(page.Tail)
	}
	return &sessionpresentation.DetailOutput{
		DetailOutputRequest: sessionpresentation.DetailOutputRequest{
			ID:      id,
			Page:    pageID,
			Version: version,
		},
		View: activityfeed.OutputView{
			Lines:  lines,
			Done:   done,
			Exited: page.Exited,
			Exit:   page.ExitCode,
		},
	}
}

func (c *directNativeConnection) handleDetailOutput(req sessionpresentation.DetailOutputRequest) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.details == nil || c.details.View.ID != req.ID {
		return
	}
	for _, p := range c.details.View.Pages {
		if p.ID != req.Page || p.OutputVersion != req.Version {
			continue
		}
		var rec *directItemRecord
		if p.Process != nil {
			rec = c.processRecordLocked(*p.Process)
		} else {
			rec = c.findRecordBySeqLocked(c.details.Source.ID, p.Source)
		}
		details := *c.details
		c.setDetailsOutputLocked(&details, req.Page, req.Version, rec, p.DetailPage)
		c.details = &details
		c.publishLocked()
		return
	}
}

func (c *directNativeConnection) openStatusPanel(ctx context.Context) error {
	var accountResp struct {
		Account *struct {
			Type  string `json:"type"`
			Email string `json:"email"`
			Plan  string `json:"planType"`
		} `json:"account"`
	}
	var limits *directRateLimitsResponse
	var limitsErr error
	if c.wire != nil {
		_ = c.wire.Call(ctx, "account/read", map[string]any{"refreshToken": false}, &accountResp)
		limits, limitsErr = c.refreshRateLimits(ctx)
	}

	c.mu.Lock()
	defer c.mu.Unlock()
	var fields []sessionpresentation.StatusField
	add := func(group, label, value string) {
		if value != "" {
			fields = append(fields, sessionpresentation.StatusField{Group: group, Label: label, Value: value})
		}
	}
	add("Model", "Model", c.model)
	add("Model", "Reasoning", c.effort)
	add("Model", "Service tier", c.tier)
	add("Model", "Model provider", c.modelProvider)
	add("Session", "Directory", c.workspace)
	add("Session", "Thread", c.mainThread)
	if c.title != "" {
		add("Session", "Title", c.title)
	}
	add("Session", "Agents.md", strings.Join(c.instructionSources, ", "))
	add("Permissions", "Approvals", c.approvalPolicy)
	add("Permissions", "Sandbox", c.sandboxPolicy)
	if u, ok := c.threadUsage[c.mainThread]; ok {
		if u.ContextWindow > 0 {
			remaining := 100 * (1 - min(1, float64(u.ContextTokens)/float64(u.ContextWindow)))
			fields = append(fields, sessionpresentation.StatusField{
				Group:     "Usage",
				Label:     "Context window",
				Value:     fmt.Sprintf("%.0f%% left", remaining),
				Detail:    fmt.Sprintf("%s used / %s", activityui.FormatUsageTokens(u.ContextTokens), activityui.FormatUsageTokens(u.ContextWindow)),
				Remaining: &remaining,
			})
		}
		if u.InputTokens > 0 || u.OutputTokens > 0 {
			add("Usage", "Token usage", fmt.Sprintf("%s input · %s output", activityui.FormatUsageTokens(u.InputTokens), activityui.FormatUsageTokens(u.OutputTokens)))
		}
		if u.CostKnown {
			prefix := "$"
			if u.CostPartial {
				prefix = "≥$"
			}
			add("Usage", "Estimated cost", fmt.Sprintf("%s%.2f", prefix, u.EstimatedCostUSD))
		}
		if label := u.throughputLabel(); label != "" {
			add("Usage", "Output speed", label)
		}
	}
	if accountResp.Account != nil {
		switch accountResp.Account.Type {
		case "chatgpt":
			add("Account", "Sign-in", "ChatGPT")
			add("Account", "Email", accountResp.Account.Email)
			if accountResp.Account.Plan != "" && accountResp.Account.Plan != "unknown" {
				add("Account", "Plan", accountResp.Account.Plan)
			}
		case "apiKey":
			add("Account", "Sign-in", "API key")
		case "amazonBedrock":
			add("Account", "Sign-in", "Amazon Bedrock")
		}
	}
	if limits != nil {
		fields = append(fields, limits.statusFields()...)
	} else if limitsErr != nil && accountResp.Account != nil && accountResp.Account.Type == "chatgpt" {
		add("Account", "Usage limits", limitsErr.Error())
	}
	c.panelSeq++
	c.panelKind = "status"
	c.panel = &sessionpresentation.PanelView{
		ID:        c.panelSeq,
		Fields:    fields,
		Workspace: c.workspace,
	}
	c.publishLocked()
	return nil
}

func (limits directRateLimitsResponse) statusFields() []sessionpresentation.StatusField {
	buckets := limits.ByID
	if len(buckets) == 0 {
		buckets = map[string]directRateLimitSnapshot{"Usage": limits.RateLimits}
	}
	const dateLayout = "Jan 2 15:04 UTC-07:00"
	var fields []sessionpresentation.StatusField
	for _, id := range slices.Sorted(maps.Keys(buckets)) {
		bucket := buckets[id]
		name := cmp.Or(bucket.Name, id)
		for position, window := range []*directRateLimitWindow{bucket.Primary, bucket.Secondary} {
			if window == nil {
				continue
			}
			remaining := max(0, min(100, 100-window.Used))
			field := sessionpresentation.StatusField{
				Group: "Account", Label: name + " " + window.label(position) + " limit",
				Value: fmt.Sprintf("%.0f%% left", remaining), Remaining: &remaining,
			}
			if window.Resets != nil {
				field.Detail = "resets " + time.Unix(*window.Resets, 0).Local().Format(dateLayout)
			}
			fields = append(fields, field)
		}
		if bucket.Credits != nil {
			var value string
			if bucket.Credits.Unlimited {
				value = "unlimited"
			} else if bucket.Credits.Balance != nil {
				value = *bucket.Credits.Balance
			}
			if value != "" {
				fields = append(fields, sessionpresentation.StatusField{Group: "Account", Label: name + " credits", Value: value})
			}
		}
	}
	if resets := limits.Resets; resets != nil {
		const group = "Usage limit resets"
		fields = append(fields, sessionpresentation.StatusField{Group: group, Label: "Available", Value: fmt.Sprint(resets.Available)})
		for _, credit := range resets.Credits {
			if credit.Status != "available" {
				continue
			}
			label := credit.Title
			if label == "" && credit.Type == "codexRateLimits" {
				label = "Full reset"
			}
			value := "Does not expire"
			if credit.Expires != nil {
				value = "Expires " + time.Unix(*credit.Expires, 0).Local().Format(dateLayout)
			}
			fields = append(fields, sessionpresentation.StatusField{Group: group, Label: cmp.Or(label, "Reset"), Value: value})
		}
	}
	return fields
}

func (c *directNativeConnection) ReadFeatureUse(ctx context.Context, threads []string) (map[string]observation.FeatureUse, error) {
	var response struct {
		Threads map[string]observation.FeatureUse `json:"threads"`
	}
	err := c.wire.Call(ctx, "featureUse/read", map[string]any{"threads": threads}, &response)
	return response.Threads, err
}

func (c *directNativeConnection) readFeatureFields(ctx context.Context, thread, name string) []sessionpresentation.StatusField {
	var response struct {
		Data []directWireThread `json:"data"`
	}
	err := c.wire.Call(ctx, "thread/subtree/list", map[string]string{"threadId": thread}, &response)
	if err != nil {
		return sessionpresentation.FeatureFields(name, sessionpresentation.FeatureSnapshot{Err: err})
	}
	var descendants []string
	for _, entry := range response.Data {
		if entry.ID != thread {
			descendants = append(descendants, entry.ID)
		}
	}
	return sessionpresentation.FeatureFields(name, sessionpresentation.ReadNativeFeatureUse(ctx, thread, descendants, c))
}

func (c *directNativeConnection) openSessionPanel(ctx context.Context) error {
	var diagnostics struct {
		DebugPath        string  `json:"debugPath"`
		ProviderRequests *uint64 `json:"providerRequests"`
	}
	diagnosticsErr := c.wire.Call(ctx, "server/diagnostics", struct{}{}, &diagnostics)
	c.mu.Lock()
	defer c.mu.Unlock()
	var fields []sessionpresentation.StatusField
	add := func(group, label, value string) {
		if value != "" {
			fields = append(fields, sessionpresentation.StatusField{Group: group, Label: label, Value: value})
		}
	}
	add("Session", "Thread", c.mainThread)
	add("Session", "Model", c.model)
	if diagnosticsErr == nil {
		add("This launch", "Debug path", diagnostics.DebugPath)
		if diagnostics.ProviderRequests != nil {
			add("This launch", "Provider requests", fmt.Sprint(*diagnostics.ProviderRequests))
		}
	}
	var totalIn, totalCached, totalOut, totalReasoning, turns, totalRoundtrips uint64
	var totalCost float64
	usageKnown, costKnown, costPartial := false, false, false
	usagePartial, roundtripsPartial := false, false
	for _, row := range c.rosterLocked() {
		a := row.Agent
		totalIn += a.InputTokens
		totalOut += a.OutputTokens
		turns += a.Turns
		totalRoundtrips += a.Roundtrips
		roundtripsPartial = roundtripsPartial || a.RoundtripsPartial
		usagePartial = usagePartial || a.UsagePartial
		if usage, ok := c.threadUsage[a.Thread]; ok && usage.TokensKnown {
			usageKnown = true
			totalCached += usage.CachedInputTokens
			totalReasoning += usage.ReasoningOutputTokens
		}
		if a.CostKnown {
			totalCost += a.Cost
			costKnown = true
		}
		costPartial = costPartial || a.CostPartial || (a.Roundtrips > 0 && !a.CostKnown)
	}
	if usageKnown {
		prefix := ""
		if usagePartial {
			prefix = "≥"
		}
		add("Usage", "Input tokens", prefix+activityui.FormatUsageTokens(totalIn))
		add("Usage", "Cached input tokens", prefix+activityui.FormatUsageTokens(totalCached))
		if totalCached <= totalIn {
			add("Usage", "Uncached input tokens", prefix+activityui.FormatUsageTokens(totalIn-totalCached))
		}
		add("Usage", "Output tokens", prefix+activityui.FormatUsageTokens(totalOut))
		add("Usage", "Reasoning tokens", prefix+activityui.FormatUsageTokens(totalReasoning))
	}
	if costKnown {
		prefix := "$"
		if costPartial {
			prefix = "≥$"
		}
		add("Usage", "Total cost", fmt.Sprintf("%s%.2f", prefix, totalCost))
	}
	if turns > 0 {
		add("Usage", "Turns", fmt.Sprintf("%d", turns))
	}
	if totalRoundtrips > 0 {
		prefix := ""
		if roundtripsPartial {
			prefix = "≥"
		}
		add("Usage", "Roundtrips", fmt.Sprintf("%s%d", prefix, totalRoundtrips))
	}
	if u, ok := c.threadUsage[c.mainThread]; ok {
		if label := u.throughputLabel(); label != "" {
			add("Usage", "Average output", label)
		}
		if u.WorkDurationMs > 0 {
			add("Usage", "Work duration", (time.Duration(u.WorkDurationMs) * time.Millisecond).Round(100*time.Millisecond).String())
		}
		if !u.LastResponse.IsZero() {
			add("Usage", "Last response", u.LastResponse.Format(time.DateTime))
		}
		if !u.LastCompleted.IsZero() {
			add("Usage", "Last completed", u.LastCompleted.Format(time.DateTime))
		}
	}
	c.panelSeq++
	c.panelKind = "session"
	c.panel = &sessionpresentation.PanelView{
		ID:        c.panelSeq,
		Session:   true,
		Fields:    fields,
		Workspace: c.workspace,
	}
	c.publishLocked()
	return nil
}

func (c *directNativeConnection) handlePanelAction(ctx context.Context, action sessionpresentation.PanelAction) error {
	c.mu.Lock()
	if c.panel == nil || (action.Panel != 0 && c.panel.ID != action.Panel) {
		c.mu.Unlock()
		return nil
	}
	kind := c.panelKind
	switch action.Kind {
	case sessionpresentation.PanelClose:
		c.panel = nil
		c.panelKind = ""
		c.publishLocked()
		c.mu.Unlock()
		return nil
	case sessionpresentation.PanelFeatures:
		if kind != "session" {
			c.mu.Unlock()
			return nil
		}
		id := c.panel.ID
		thread := cmp.Or(c.displayedThread, c.mainThread)
		name := pathdisplay.AgentDisplayName(c.threadCallers[thread])
		if thread == c.mainThread {
			name = "Main"
		}
		c.mu.Unlock()
		fields := c.readFeatureFields(ctx, thread, name)
		c.mu.Lock()
		defer c.mu.Unlock()
		if c.panel != nil && c.panel.ID == id && thread == cmp.Or(c.displayedThread, c.mainThread) {
			c.panel.FeatureFields = fields
			c.publishLocked()
		}
		return nil
	case sessionpresentation.PanelRefresh:
		c.mu.Unlock()
		if kind == "session" {
			return c.openSessionPanel(ctx)
		}
		return c.openStatusPanel(ctx)
	default:
		c.mu.Unlock()
		return nil
	}
}

func (c *directNativeConnection) handleReference(ref sessionpresentation.ReferenceIntent) {
	c.mu.Lock()
	defer c.mu.Unlock()
	threadID := ref.Thread
	if threadID == "" {
		threadID = c.inputThread
	}
	if threadID == "" {
		threadID = c.mainThread
	}
	target := ref.Target
	c.effectSeq++
	c.effects = append(c.effects, sessionapi.NativeEffect{
		ID:     c.effectSeq,
		Thread: threadID,
		Target: &target,
		Selection: &sessionapi.NativeSelection{
			Description: ref.Description,
			Text:        ref.Text,
			Source:      ref.Source,
		},
	})
	c.publishLocked()
}

func (c *directNativeConnection) handleNotice(action sessionapi.NativeNoticeAction) {
	c.mu.Lock()
	defer c.mu.Unlock()
	current := sessionpresentation.Version{Attachment: c.attachID, Revision: c.revision}
	if !action.Version.SameAttachment(current) || action.Feedback != c.feedbackRev || c.feedbackText == "" {
		return
	}
	if action.Dismiss {
		c.setFeedbackLocked("", false)
		c.publishLocked()
		return
	}
	c.publishErrorDetailsLocked(c.mainThread, false,
		[]activity.Value{{Content: activityfeed.Content{Kind: "error", Body: c.feedbackText}}})
}

func (c *directNativeConnection) emitPresentationEffectLocked(feedback string) {
	c.effectSeq++
	c.effects = append(c.effects, sessionapi.NativeEffect{
		ID:     c.effectSeq,
		Thread: c.mainThread,
		Presentation: &sessionapi.NativePresentationEffect{
			LiveHidden:          c.liveHidden,
			InterruptLocked:     c.interruptLocked,
			PredictionsDisabled: c.predictionsDisabled,
		},
	})
	if feedback != "" {
		c.setFeedbackLocked(feedback, false)
	}
	c.publishLocked()
}

func (c *directNativeConnection) forkAndSwitchThread(ctx context.Context, promptText string) error {
	c.mu.Lock()
	mainID := c.mainThread
	c.mu.Unlock()
	if mainID == "" {
		return errors.New("no active thread")
	}
	var resp directThreadStartResponse
	if err := c.wire.Call(ctx, "thread/fork", map[string]any{"threadId": mainID}, &resp); err != nil {
		return fmt.Errorf("fork thread: %w", err)
	}
	c.mu.Lock()
	c.applyThreadBootstrapLocked(resp)
	c.hydrateThreadTurnsLocked(resp.Thread.ID, resp.Thread.Turns)
	c.setFeedbackLocked("Switched to forked thread", false)
	c.publishLocked()
	c.mu.Unlock()
	c.refreshInputHistory(ctx, resp.Thread.ID)
	c.refreshAuxiliary(ctx, resp.Thread.ID)
	if promptText != "" {
		return c.startInputTurn(ctx, resp.Thread.ID, appserver.Input(promptText))
	}
	return nil
}
