// Port of mj-client/src/settings/discovery.rs.

// Choices a draft offers only once they are discovered: the models and
// efforts its profiles advertise, from the daemon's capability catalog.

package client

import (
	"fmt"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// EffortsPending is set on a task's discovered choices while its selected
// model's efforts are still being discovered.
const EffortsPending = "efforts_pending"

// FillSubagentOptions fills the sub-agent model and effort choices of draft
// from capabilities. A task whose profiles are not all discovered yet is
// left unset, which the page reports as still discovering. Returns what
// discovery could not reach; a draft that is not a configuration yet is
// left as it is.
func FillSubagentOptions(draft *any, capabilities *core.ProfileCapabilitiesSnapshot) (*string, error) {
	config, err := ConfigFromDraft(*draft)
	if err != nil {
		return nil, err
	}
	var unreachable *string
	baseModels := capabilities.Options(&config, nil)
	root := jsonvalue.Object{}
	for _, task := range settingsTasks {
		taskOptions := jsonvalue.Object{}
		if entries, ok := settingsArray(settingsPath(*draft, "subagents", task)); ok {
			for index, entry := range entries {
				var model *string
				if text, ok := settingsString(settingsGet(entry, "model")); ok && strings.TrimSpace(text) != "" {
					model = &text
				}
				options := cloneSubagentOptions(baseModels)
				effortsPending := false
				if options != nil {
					if efforts := capabilities.Options(&config, model); efforts != nil {
						options.Efforts = efforts.Efforts
						options.Unavailable = efforts.Unavailable
					} else {
						effortsPending = true
					}
				}
				var value any
				if options != nil {
					if len(options.Unavailable) > 0 && unreachable == nil {
						text := "Discovery: " + strings.Join(options.Unavailable, "; ")
						unreachable = &text
					}
					value = subagentOptionsValue(options)
					if effortsPending || unavailablePending(value) {
						object := value.(jsonvalue.Object)
						object.Set(EffortsPending, true)
						value = object
					}
				}
				taskOptions.Set(strconv.Itoa(index), value)
			}
		}
		root.Set(task, taskOptions)
	}
	settingsSet(draft, root, "_subagent_options")
	return unreachable, nil
}

// cloneSubagentOptions is Option<SubagentOptions>::clone.
func cloneSubagentOptions(options *core.SubagentOptions) *core.SubagentOptions {
	if options == nil {
		return nil
	}
	cloned := core.SubagentOptions{
		Models:      slices.Clone(options.Models),
		Efforts:     slices.Clone(options.Efforts),
		Unavailable: slices.Clone(options.Unavailable),
	}
	return &cloned
}

// subagentOptionsValue is serde_json::to_value of the options, with each list
// written as serde writes a Vec.
func subagentOptionsValue(options *core.SubagentOptions) any {
	if options == nil {
		return nil
	}
	normalized := *options
	if normalized.Models == nil {
		normalized.Models = []core.SessionConfigChoice{}
	}
	if normalized.Efforts == nil {
		normalized.Efforts = []core.SessionConfigChoice{}
	}
	if normalized.Unavailable == nil {
		normalized.Unavailable = []string{}
	}
	value, err := settingsToValue(normalized)
	if err != nil {
		panic(fmt.Sprintf("options serialize: %v", err))
	}
	return value
}

// unavailablePending is whether an options value names a reason that
// discovery is still pending.
func unavailablePending(value any) bool {
	reasons, ok := settingsArray(settingsGet(value, "unavailable"))
	if !ok {
		return false
	}
	for _, reason := range reasons {
		if text, ok := settingsString(reason); ok && strings.Contains(text, "pending") {
			return true
		}
	}
	return false
}

// FillReviewerOptions fills reviewer choices from the same profile catalog
// as subagent choices.
func FillReviewerOptions(draft *any, capabilities *core.ProfileCapabilitiesSnapshot) error {
	config, err := ConfigFromDraft(*draft)
	if err != nil {
		return err
	}
	reviewers, ok := settingsArray(settingsPath(*draft, "review", "reviewers"))
	if !ok {
		return nil
	}
	models := capabilities.Options(&config, nil)
	options := jsonvalue.Object{}
	for index, reviewer := range reviewers {
		var model *string
		if text, ok := settingsString(settingsGet(reviewer, "model")); ok && text != "" {
			model = &text
		}
		current := cloneSubagentOptions(models)
		if current != nil {
			efforts := capabilities.Options(&config, model)
			if efforts == nil {
				value := subagentOptionsValue(current).(jsonvalue.Object)
				value.Set(EffortsPending, true)
				options.Set(strconv.Itoa(index), value)
				continue
			}
			current.Efforts = efforts.Efforts
			current.Unavailable = efforts.Unavailable
		}
		value := subagentOptionsValue(current)
		if unavailablePending(value) {
			object := value.(jsonvalue.Object)
			object.Set(EffortsPending, true)
			value = object
		}
		options.Set(strconv.Itoa(index), value)
	}
	settingsSet(draft, options, "_reviewer_options")
	return nil
}
