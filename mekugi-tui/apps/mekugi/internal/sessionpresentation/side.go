package sessionpresentation

import activity "github.com/mekugi-harness/mekugi/mekugi-tui/modules/activity/presentation"

const SideCaller = "/oneoff"

type SideToken struct {
	ID     uint64
	Key    string
	Label  string
	Skills activity.SkillSet
}

type CloseSideIntent struct {
	Version  Version
	Key      string
	Exchange uint64
}
