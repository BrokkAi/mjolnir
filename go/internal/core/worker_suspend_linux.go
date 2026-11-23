// Port of mj-core/src/worker_suspend.rs: WorkerIdentity::is_alive on Linux.

package core

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"strconv"
	"strings"
)

func workerIdentityIsAlive(w WorkerIdentity) (bool, error) {
	stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", w.Pid))
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	text := string(stat)
	closing := strings.LastIndex(text, ")")
	if closing < 0 {
		return false, errors.New("malformed worker stat")
	}
	fields := strings.Fields(text[closing+1:])
	if len(fields) == 0 {
		return false, errors.New("worker stat is missing its process state")
	}
	state := fields[0]
	if len(fields) <= 19 {
		return false, errors.New("worker stat is missing its birth identity")
	}
	start, err := strconv.ParseUint(fields[19], 10, 64)
	if err != nil {
		return false, err
	}
	return start == w.Start && state != "Z" && state != "X", nil
}
