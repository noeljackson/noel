package resources

import (
	"fmt"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

type RenderArgs map[string]string

func (t PromptTemplate) Render(args RenderArgs) (string, error) {
	resolved := make(map[string]string, len(args)+len(t.Args))
	for key, value := range args {
		resolved[key] = value
	}
	for _, arg := range t.Args {
		if resolved[arg.Name] == "" && arg.Default != "" {
			resolved[arg.Name] = arg.Default
		}
		if arg.Required && resolved[arg.Name] == "" {
			return "", fmt.Errorf("missing required argument: %s", arg.Name)
		}
	}

	result := t.Body
	for _, arg := range t.Args {
		value := resolved[arg.Name]
		result = strings.ReplaceAll(result, "{{"+arg.Name+"}}", value)
		result = strings.ReplaceAll(result, "${"+arg.Name+"}", value)
	}

	ordered := t.orderedArgValues(resolved)
	result = substitutePositionalArgs(result, ordered)
	return result, nil
}

func (t PromptTemplate) orderedArgValues(args map[string]string) []string {
	if len(t.Args) > 0 {
		out := make([]string, 0, len(t.Args))
		for _, arg := range t.Args {
			out = append(out, args[arg.Name])
		}
		return out
	}
	keys := make([]string, 0, len(args))
	for key := range args {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	out := make([]string, 0, len(keys))
	for _, key := range keys {
		out = append(out, args[key])
	}
	return out
}

func substitutePositionalArgs(content string, args []string) string {
	positional := regexp.MustCompile(`\$(\d+)`)
	result := positional.ReplaceAllStringFunc(content, func(match string) string {
		index, err := strconv.Atoi(match[1:])
		if err != nil || index <= 0 || index > len(args) {
			return ""
		}
		return args[index-1]
	})

	slice := regexp.MustCompile(`\$\{@:(\d+)(?::(\d+))?\}`)
	result = slice.ReplaceAllStringFunc(result, func(match string) string {
		parts := slice.FindStringSubmatch(match)
		start, _ := strconv.Atoi(parts[1])
		if start <= 0 {
			start = 1
		}
		start--
		if start > len(args) {
			return ""
		}
		if parts[2] != "" {
			length, _ := strconv.Atoi(parts[2])
			end := start + length
			if end > len(args) {
				end = len(args)
			}
			return strings.Join(args[start:end], " ")
		}
		return strings.Join(args[start:], " ")
	})

	all := strings.Join(args, " ")
	result = strings.ReplaceAll(result, "$ARGUMENTS", all)
	result = strings.ReplaceAll(result, "$@", all)
	return result
}

func parseTemplateArgs(value any) []TemplateArg {
	switch typed := value.(type) {
	case []any:
		out := make([]TemplateArg, 0, len(typed))
		for _, item := range typed {
			switch arg := item.(type) {
			case string:
				out = append(out, TemplateArg{Name: arg, Required: true})
			case map[string]any:
				out = append(out, templateArgFromMap(arg))
			}
		}
		return out
	case map[string]any:
		keys := make([]string, 0, len(typed))
		for key := range typed {
			keys = append(keys, key)
		}
		sort.Strings(keys)
		out := make([]TemplateArg, 0, len(keys))
		for _, key := range keys {
			arg := TemplateArg{Name: key}
			if nested, ok := typed[key].(map[string]any); ok {
				arg.Required = boolValue(nested["required"])
				arg.Default = stringValue(nested["default"])
			} else {
				arg.Required = boolValue(typed[key])
			}
			out = append(out, arg)
		}
		return out
	default:
		return nil
	}
}

func templateArgFromMap(values map[string]any) TemplateArg {
	return TemplateArg{
		Name:     stringValue(values["name"]),
		Required: boolValue(values["required"]),
		Default:  stringValue(values["default"]),
	}
}

func boolValue(value any) bool {
	if b, ok := value.(bool); ok {
		return b
	}
	return false
}
