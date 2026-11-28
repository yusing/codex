package presentation

import (
	"time"

	"github.com/mekugi-harness/mekugi/mekugi-tui/contracts/inference"
	"github.com/mekugi-harness/mekugi/mekugi-tui/contracts/observation"

	"github.com/mekugi-harness/mekugi/mekugi-tui/libraries/diffmodel"
)

// Source identifies an application-supplied observation without interpreting host protocol.
// The original typed source stays on its record; Facts is a transient semantic projection.
type Source interface{ ActivityFacts() Facts }
type Facts struct {
	Images                                                []string `json:"Images,omitzero"` // Explicitly bound image sources; public projection uses opaque identities.
	Retained                                              bool     // Bounded projections retired explicitly by their source owner.
	TextRef                                               observation.ItemRef
	Seq                                                   uint64
	Agent, Kind, Text, Outcome                            string
	Observed                                              time.Time
	Spans                                                 []TextSpan
	InputGroup                                            string
	Question, Activity                                    uint64
	AssignmentTo, AssignmentText                          string
	Assignment                                            bool
	Delivered                                             bool // Attributed mail received from another runtime.
	Running, Live, Restored, Unconfirmed, ContextBoundary bool
	Suppressed, OutputFailed, OutputComplete              bool
	ItemComplete                                          bool
	Completed                                             time.Time
	Outputs                                               map[int]int
}
type AgentSource interface{ ActivityAgent() AgentFacts }
type AgentFacts struct {
	Generating                                   string
	Name, Role, Thread                           string
	NonAttachable                                bool
	WaitingFor                                   string
	Wait                                         *Value
	Responding, Final, Waiting, Paused           bool
	Compacting                                   bool
	Retrying                                     bool
	Started, LastResponse                        time.Time
	WorkElapsed                                  time.Duration
	WorkSince                                    time.Time
	WorkKnown                                    bool
	Turns, Roundtrips                            uint64
	Cost                                         float64
	CostKnown, CostPartial                       bool
	InputTokens, OutputTokens                    uint64
	OutputThroughput                             inference.OutputThroughput
	TokensKnown, UsagePartial, RoundtripsPartial bool
	ContextTokens, ContextWindow                 uint64
	ContextKnown                                 bool
}

// Source: internal/router/live_activity_record.go:9:87@[ebde9ad5].
// Record keeps one original observation, its neutral values and revision together.
// Source is private adapter evidence, never a view publication.
type Record[S Source] struct {
	Source   S
	Values   []Value
	Revision uint64
}

// Model owns one semantic feed. The renderer keeps disclosure, animation, layout and selection.
// Main and child feeds share an attached Session but never duplicate their records.
type Model[S Source, A AgentSource] struct {
	Entries           []Record[S]
	Agents            []A
	Revision, LastSeq uint64
	LineCounts        map[string]diffmodel.Counts
	NetCounts         *diffmodel.Counts
}

// ObservedStatus keeps detached roster publications consistent with feed models.
func (facts AgentFacts) ObservedStatus(latestKind string) string {
	if facts.Paused {
		return "paused"
	}
	if facts.Waiting {
		return "waiting"
	}
	if facts.Responding {
		return "responding"
	}
	if latestKind == "error" {
		return "error"
	}
	if facts.Final {
		return "answered"
	}
	return ""
}

// Identify moves stable entry and operation identity with flat group membership.
func Identify(values []Value, source uint64) {
	for i := range values {
		values[i].Source = source
		if values[i].Operation == 0 {
			values[i].Operation = i + 1
		}
		for j := range values[i].Members {
			if values[i].Members[j].Operation == values[i].Operation {
				original := values[i]
				original.Members = nil
				values[i].Members[j] = original
			}
			values[i].Members[j].Source = source
		}
	}
}

// Summary selects the latest meaningful observation, including a root-directed
// message. Earlier reasoning sections stay tied to their own agent across traffic.
type Summary struct {
	Index    int
	Earlier  []int
	Incoming bool
}
