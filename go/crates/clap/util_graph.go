// Port of clap_builder 4.6.0 src/util/graph.rs

package clap

type utilChild struct {
	id       Id
	children []int
}

// utilChildGraph is clap's ChildGraph<Id>.
type utilChildGraph struct {
	nodes []utilChild
}

func utilChildGraphWithCapacity(s int) *utilChildGraph {
	return &utilChildGraph{nodes: make([]utilChild, 0, s)}
}

func (g *utilChildGraph) insert(req Id) int {
	for index, node := range g.nodes {
		if node.id == req {
			return index
		}
	}
	idx := len(g.nodes)
	g.nodes = append(g.nodes, utilChild{id: req})
	return idx
}

func (g *utilChildGraph) insertChild(parent int, child Id) int {
	cIdx := len(g.nodes)
	g.nodes = append(g.nodes, utilChild{id: child})
	g.nodes[parent].children = append(g.nodes[parent].children, cIdx)
	return cIdx
}

func (g *utilChildGraph) iter() []Id {
	ids := make([]Id, len(g.nodes))
	for i, node := range g.nodes {
		ids[i] = node.id
	}
	return ids
}

func (g *utilChildGraph) contains(req Id) bool {
	for _, node := range g.nodes {
		if node.id == req {
			return true
		}
	}
	return false
}
