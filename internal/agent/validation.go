package agent

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"reflect"
	"strconv"
	"strings"
)

// ValidateToolArguments validates raw JSON arguments against a tool's schema.
func ValidateToolArguments(schema json.RawMessage, args json.RawMessage) error {
	_, err := ValidateAndCoerceToolArguments(schema, args)
	return err
}

// ValidateAndCoerceToolArguments validates and returns JSON arguments after primitive coercion.
func ValidateAndCoerceToolArguments(schema json.RawMessage, args json.RawMessage) (json.RawMessage, error) {
	var schemaValue interface{}
	if err := decodeJSON(schema, &schemaValue); err != nil {
		return nil, fmt.Errorf("validation: schema is invalid: %w", err)
	}
	schemaObject, ok := schemaValue.(map[string]interface{})
	if !ok {
		return nil, fmt.Errorf("validation: schema must be an object")
	}

	var argsValue interface{}
	if err := decodeJSON(args, &argsValue); err != nil {
		return nil, fmt.Errorf("validation: arguments are invalid JSON: %w", err)
	}

	coerced := coerceValue(argsValue, schemaObject)
	if err := validateValue(coerced, schemaObject, "root"); err != nil {
		return nil, err
	}
	encoded, err := json.Marshal(coerced)
	if err != nil {
		return nil, fmt.Errorf("validation: arguments could not be encoded: %w", err)
	}
	return encoded, nil
}

func decodeJSON(data json.RawMessage, target *interface{}) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	return decoder.Decode(target)
}

func coerceValue(value interface{}, schema map[string]interface{}) interface{} {
	next := value
	for _, key := range []string{"anyOf", "oneOf"} {
		if schemas, ok := schemaArray(schema[key]); ok {
			if coerced, ok := coerceUnion(next, schemas); ok {
				next = coerced
			}
		}
	}

	types := schemaTypes(schema)
	if len(types) > 0 && !unionAlreadyMatches(next, types) {
		for _, schemaType := range types {
			coerced := coercePrimitive(next, schemaType)
			if !sameJSONValue(coerced, next) {
				next = coerced
				break
			}
		}
	}

	if hasType(types, "object") {
		if object, ok := next.(map[string]interface{}); ok {
			coerceObject(object, schema)
		}
	}
	if hasType(types, "array") {
		if array, ok := next.([]interface{}); ok {
			coerceArray(array, schema)
		}
	}
	return next
}

func coerceUnion(value interface{}, schemas []map[string]interface{}) (interface{}, bool) {
	for _, schema := range schemas {
		candidate := cloneJSONValue(value)
		coerced := coerceValue(candidate, schema)
		if validateValue(coerced, schema, "root") == nil {
			return coerced, true
		}
	}
	return value, false
}

func coerceObject(object map[string]interface{}, schema map[string]interface{}) {
	properties, _ := objectSchemas(schema["properties"])
	for key, propertySchema := range properties {
		if value, ok := object[key]; ok {
			object[key] = coerceValue(value, propertySchema)
		}
	}

	additional, ok := schema["additionalProperties"].(map[string]interface{})
	if !ok {
		return
	}
	for key, value := range object {
		if _, defined := properties[key]; defined {
			continue
		}
		object[key] = coerceValue(value, additional)
	}
}

func coerceArray(array []interface{}, schema map[string]interface{}) {
	switch items := schema["items"].(type) {
	case map[string]interface{}:
		for index, value := range array {
			array[index] = coerceValue(value, items)
		}
	case []interface{}:
		for index, value := range array {
			if index >= len(items) {
				break
			}
			itemSchema, ok := items[index].(map[string]interface{})
			if !ok {
				continue
			}
			array[index] = coerceValue(value, itemSchema)
		}
	}
}

func coercePrimitive(value interface{}, schemaType string) interface{} {
	switch schemaType {
	case "number":
		switch typed := value.(type) {
		case nil:
			return json.Number("0")
		case string:
			trimmed := strings.TrimSpace(typed)
			if trimmed == "" {
				return value
			}
			if parsed, err := strconv.ParseFloat(trimmed, 64); err == nil && !math.IsInf(parsed, 0) && !math.IsNaN(parsed) {
				return json.Number(trimmed)
			}
		case bool:
			if typed {
				return json.Number("1")
			}
			return json.Number("0")
		}
	case "integer":
		switch typed := value.(type) {
		case nil:
			return json.Number("0")
		case string:
			trimmed := strings.TrimSpace(typed)
			if trimmed == "" {
				return value
			}
			if parsed, err := strconv.ParseFloat(trimmed, 64); err == nil && math.Trunc(parsed) == parsed {
				return json.Number(strconv.FormatInt(int64(parsed), 10))
			}
		case bool:
			if typed {
				return json.Number("1")
			}
			return json.Number("0")
		}
	case "boolean":
		switch typed := value.(type) {
		case nil:
			return false
		case string:
			if typed == "true" {
				return true
			}
			if typed == "false" {
				return false
			}
		case json.Number:
			if typed.String() == "1" {
				return true
			}
			if typed.String() == "0" {
				return false
			}
		}
	case "string":
		switch typed := value.(type) {
		case nil:
			return ""
		case json.Number:
			return typed.String()
		case bool:
			return strconv.FormatBool(typed)
		}
	case "null":
		switch typed := value.(type) {
		case string:
			if typed == "" {
				return nil
			}
		case json.Number:
			if typed.String() == "0" {
				return nil
			}
		case bool:
			if !typed {
				return nil
			}
		}
	}
	return value
}

func validateValue(value interface{}, schema map[string]interface{}, path string) error {
	for _, key := range []string{"anyOf", "oneOf"} {
		if err := validateUnion(value, schema, path, key); err != nil {
			return err
		}
	}

	types := schemaTypes(schema)
	if len(types) == 0 {
		types = inferredTypes(schema)
	}
	if len(types) > 0 {
		matched := false
		for _, schemaType := range types {
			if matchesType(value, schemaType) {
				matched = true
				break
			}
		}
		if !matched {
			return validationError(path, "must be "+strings.Join(types, " or ")+", got "+jsonType(value))
		}
	}

	if err := validateEnum(value, schema, path); err != nil {
		return err
	}
	if err := validateRange(value, schema, path); err != nil {
		return err
	}
	if err := validateLength(value, schema, path); err != nil {
		return err
	}
	if object, ok := value.(map[string]interface{}); ok {
		if err := validateObject(object, schema, path); err != nil {
			return err
		}
	}
	if array, ok := value.([]interface{}); ok {
		if err := validateArray(array, schema, path); err != nil {
			return err
		}
	}
	return nil
}

func validateUnion(value interface{}, schema map[string]interface{}, path string, key string) error {
	schemas, ok := schemaArray(schema[key])
	if !ok {
		return nil
	}
	matches := 0
	var firstErr error
	for _, nested := range schemas {
		if err := validateValue(value, nested, path); err == nil {
			matches++
		} else if firstErr == nil {
			firstErr = err
		}
	}
	if key == "anyOf" && matches == 0 {
		if firstErr != nil {
			return firstErr
		}
		return validationError(path, "must match at least one schema")
	}
	if key == "oneOf" && matches != 1 {
		if matches == 0 && firstErr != nil {
			return firstErr
		}
		return validationError(path, "must match exactly one schema")
	}
	return nil
}

func validateObject(object map[string]interface{}, schema map[string]interface{}, path string) error {
	properties, _ := objectSchemas(schema["properties"])
	for _, key := range requiredProperties(schema) {
		if _, ok := object[key]; !ok {
			return validationError(joinPath(path, key), "is required")
		}
	}
	for key, value := range object {
		propertySchema, ok := properties[key]
		if ok {
			if err := validateValue(value, propertySchema, joinPath(path, key)); err != nil {
				return err
			}
			continue
		}
		switch additional := schema["additionalProperties"].(type) {
		case bool:
			if !additional {
				return validationError(joinPath(path, key), "additional properties are not allowed")
			}
		case map[string]interface{}:
			if err := validateValue(value, additional, joinPath(path, key)); err != nil {
				return err
			}
		}
	}
	return nil
}

func validateArray(array []interface{}, schema map[string]interface{}, path string) error {
	if minimum, ok := numberKeyword(schema, "minItems"); ok && float64(len(array)) < minimum {
		return validationError(path, "must contain at least "+formatNumber(minimum)+" item(s)")
	}
	if maximum, ok := numberKeyword(schema, "maxItems"); ok && float64(len(array)) > maximum {
		return validationError(path, "must contain at most "+formatNumber(maximum)+" item(s)")
	}
	switch items := schema["items"].(type) {
	case map[string]interface{}:
		for index, value := range array {
			if err := validateValue(value, items, fmt.Sprintf("%s[%d]", path, index)); err != nil {
				return err
			}
		}
	case []interface{}:
		for index, value := range array {
			if index >= len(items) {
				break
			}
			itemSchema, ok := items[index].(map[string]interface{})
			if !ok {
				continue
			}
			if err := validateValue(value, itemSchema, fmt.Sprintf("%s[%d]", path, index)); err != nil {
				return err
			}
		}
	}
	return nil
}

func validateEnum(value interface{}, schema map[string]interface{}, path string) error {
	values, ok := schema["enum"].([]interface{})
	if !ok {
		return nil
	}
	for _, enumValue := range values {
		if sameJSONValue(value, enumValue) {
			return nil
		}
	}
	return validationError(path, "must be one of "+formatEnum(values))
}

func validateRange(value interface{}, schema map[string]interface{}, path string) error {
	number, ok := numberValue(value)
	if !ok {
		return nil
	}
	if minimum, ok := numberKeyword(schema, "minimum"); ok && number < minimum {
		return validationError(path, "must be >= "+formatNumber(minimum))
	}
	if maximum, ok := numberKeyword(schema, "maximum"); ok && number > maximum {
		return validationError(path, "must be <= "+formatNumber(maximum))
	}
	return nil
}

func validateLength(value interface{}, schema map[string]interface{}, path string) error {
	text, ok := value.(string)
	if !ok {
		return nil
	}
	if minimum, ok := numberKeyword(schema, "minLength"); ok && float64(len(text)) < minimum {
		return validationError(path, "length must be >= "+formatNumber(minimum))
	}
	if maximum, ok := numberKeyword(schema, "maxLength"); ok && float64(len(text)) > maximum {
		return validationError(path, "length must be <= "+formatNumber(maximum))
	}
	return nil
}

func schemaTypes(schema map[string]interface{}) []string {
	switch typed := schema["type"].(type) {
	case string:
		return []string{typed}
	case []interface{}:
		result := make([]string, 0, len(typed))
		for _, value := range typed {
			if text, ok := value.(string); ok {
				result = append(result, text)
			}
		}
		return result
	default:
		return nil
	}
}

func inferredTypes(schema map[string]interface{}) []string {
	if _, ok := schema["properties"]; ok {
		return []string{"object"}
	}
	if _, ok := schema["required"]; ok {
		return []string{"object"}
	}
	if _, ok := schema["items"]; ok {
		return []string{"array"}
	}
	return nil
}

func matchesType(value interface{}, schemaType string) bool {
	switch schemaType {
	case "object":
		_, ok := value.(map[string]interface{})
		return ok
	case "array":
		_, ok := value.([]interface{})
		return ok
	case "string":
		_, ok := value.(string)
		return ok
	case "number":
		_, ok := numberValue(value)
		return ok
	case "integer":
		number, ok := numberValue(value)
		return ok && math.Trunc(number) == number
	case "boolean":
		_, ok := value.(bool)
		return ok
	case "null":
		return value == nil
	default:
		return false
	}
}

func jsonType(value interface{}) string {
	switch value.(type) {
	case nil:
		return "null"
	case map[string]interface{}:
		return "object"
	case []interface{}:
		return "array"
	case string:
		return "string"
	case json.Number:
		return "number"
	case bool:
		return "boolean"
	default:
		return fmt.Sprintf("%T", value)
	}
}

func validationError(path string, reason string) error {
	if path == "" {
		path = "root"
	}
	return fmt.Errorf("validation: field %q: %s", path, reason)
}

func joinPath(base string, key string) string {
	if base == "" || base == "root" {
		return key
	}
	return base + "." + key
}

func hasType(types []string, schemaType string) bool {
	for _, value := range types {
		if value == schemaType {
			return true
		}
	}
	return false
}

func unionAlreadyMatches(value interface{}, types []string) bool {
	return len(types) > 1 && anyTypeMatches(value, types)
}

func anyTypeMatches(value interface{}, types []string) bool {
	for _, schemaType := range types {
		if matchesType(value, schemaType) {
			return true
		}
	}
	return false
}

func objectSchemas(value interface{}) (map[string]map[string]interface{}, bool) {
	object, ok := value.(map[string]interface{})
	if !ok {
		return nil, false
	}
	result := make(map[string]map[string]interface{}, len(object))
	for key, raw := range object {
		if schema, ok := raw.(map[string]interface{}); ok {
			result[key] = schema
		}
	}
	return result, true
}

func schemaArray(value interface{}) ([]map[string]interface{}, bool) {
	raw, ok := value.([]interface{})
	if !ok {
		return nil, false
	}
	result := make([]map[string]interface{}, 0, len(raw))
	for _, item := range raw {
		schema, ok := item.(map[string]interface{})
		if !ok {
			continue
		}
		result = append(result, schema)
	}
	return result, len(result) > 0
}

func requiredProperties(schema map[string]interface{}) []string {
	raw, ok := schema["required"].([]interface{})
	if !ok {
		return nil
	}
	result := make([]string, 0, len(raw))
	for _, value := range raw {
		if text, ok := value.(string); ok {
			result = append(result, text)
		}
	}
	return result
}

func numberKeyword(schema map[string]interface{}, key string) (float64, bool) {
	return numberValue(schema[key])
}

func numberValue(value interface{}) (float64, bool) {
	switch typed := value.(type) {
	case json.Number:
		number, err := typed.Float64()
		return number, err == nil
	case float64:
		return typed, true
	case int:
		return float64(typed), true
	default:
		return 0, false
	}
}

func sameJSONValue(a interface{}, b interface{}) bool {
	if aNumber, ok := numberValue(a); ok {
		if bNumber, ok := numberValue(b); ok {
			return aNumber == bNumber
		}
	}
	return reflect.DeepEqual(a, b)
}

func cloneJSONValue(value interface{}) interface{} {
	encoded, err := json.Marshal(value)
	if err != nil {
		return value
	}
	var cloned interface{}
	if err := decodeJSON(encoded, &cloned); err != nil {
		return value
	}
	return cloned
}

func formatEnum(values []interface{}) string {
	parts := make([]string, 0, len(values))
	for _, value := range values {
		encoded, err := json.Marshal(value)
		if err != nil {
			parts = append(parts, fmt.Sprint(value))
			continue
		}
		parts = append(parts, string(encoded))
	}
	return strings.Join(parts, ", ")
}

func formatNumber(number float64) string {
	if math.Trunc(number) == number {
		return strconv.FormatInt(int64(number), 10)
	}
	return strconv.FormatFloat(number, 'f', -1, 64)
}
