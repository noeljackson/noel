package editor

type killRing struct {
	ring []string
}

func (k *killRing) push(text string, prepend bool, accumulate bool) {
	if text == "" {
		return
	}
	if accumulate && len(k.ring) > 0 {
		last := k.ring[len(k.ring)-1]
		if prepend {
			k.ring[len(k.ring)-1] = text + last
		} else {
			k.ring[len(k.ring)-1] = last + text
		}
		return
	}
	k.ring = append(k.ring, text)
	if len(k.ring) > 60 {
		k.ring = k.ring[len(k.ring)-60:]
	}
}

func (k *killRing) peek() (string, bool) {
	if len(k.ring) == 0 {
		return "", false
	}
	return k.ring[len(k.ring)-1], true
}

func (k *killRing) rotate() {
	if len(k.ring) <= 1 {
		return
	}
	last := k.ring[len(k.ring)-1]
	copy(k.ring[1:], k.ring[:len(k.ring)-1])
	k.ring[0] = last
}

func (k *killRing) len() int {
	return len(k.ring)
}
