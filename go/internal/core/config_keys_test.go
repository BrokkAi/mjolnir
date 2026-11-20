// Port of the tests in mj-core/src/config/keys.rs.

package core

import (
	"reflect"
	"testing"
)

func TestSimpleModeCommandsAreDirectAndNeverBindCtrlB(t *testing.T) {
	keys := SimpleKeybinds()
	if got := keys.Labels(KeyActionDetach); !reflect.DeepEqual(got, []string{"ctrl+q"}) {
		t.Fatalf("detach = %v", got)
	}
	if got := keys.Labels(KeyActionNewSession); !reflect.DeepEqual(got, []string{"ctrl+n"}) {
		t.Fatalf("new session = %v", got)
	}

	ctrlB := NewKeyCombo(KeyNameChar{'b'}, ModifiersCtrl)
	if _, ok := keys.ResolveDirect(ctrlB); ok {
		t.Fatal("ctrl+b resolves directly")
	}
	if _, ok := keys.ResolvePrefix(ctrlB); ok {
		t.Fatal("ctrl+b resolves after the prefix")
	}
	for _, action := range KeyActionAll {
		for _, binding := range keys.Bindings(action) {
			if binding.Trigger != TriggerDirect || binding.Combo == ctrlB {
				t.Fatalf("keys.%s = %+v", action.FieldName(), binding)
			}
		}
	}
	defaults := DefaultKeybinds()
	for _, action := range KeyActionAll {
		if len(defaults.Bindings(action)) != 0 && len(keys.Bindings(action)) == 0 {
			t.Fatalf("keys.%s has a default binding and none in simple mode", action.FieldName())
		}
	}
}
