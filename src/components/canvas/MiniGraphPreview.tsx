import { useEffect, useState, useMemo, useCallback } from 'react';
import * as d3 from 'd3-force';
import { getAtomNeighborhood, type NeighborhoodGraph } from '../../lib/api';
import { useUIStore } from '../../stores/ui';

interface SimulationNode extends d3.SimulationNodeDatum {
  id: string;
  depth: number;
  label: string;
}

interface MiniGraphPreviewProps {
  atomId: string;
  onExpand?: (opts?: { newTab?: boolean }) => void;
}

export function MiniGraphPreview({ atomId, onExpand }: MiniGraphPreviewProps) {
  const openLocalGraph = useUIStore(s => s.openLocalGraph);
  const [graph, setGraph] = useState<NeighborhoodGraph | null>(null);
  const [nodes, setNodes] = useState<SimulationNode[]>([]);
  const [isLoading, setIsLoading] = useState(true);

  // Fetch neighborhood data
  useEffect(() => {
    let mounted = true;

    async function fetchGraph() {
      setIsLoading(true);
      try {
        const data = await getAtomNeighborhood(atomId, 1, 0.5);
        if (mounted) {
          setGraph(data);
        }
      } catch (err) {
        console.error('Failed to load mini graph:', err);
      } finally {
        if (mounted) {
          setIsLoading(false);
        }
      }
    }

    fetchGraph();

    return () => {
      mounted = false;
    };
  }, [atomId]);

  // Run force simulation
  useEffect(() => {
    if (!graph || graph.atoms.length <= 1) return;

    const centerX = 100;
    const centerY = 100;

    // Initialize nodes (limit to 7 for mini preview)
    const limitedAtoms = graph.atoms.slice(0, 7);
    const initialNodes: SimulationNode[] = limitedAtoms.map((atom) => {
      const firstLine = atom.content.split('\n')[0].trim().replace(/^#+\s*/, '');
      const label = firstLine.length > 15 ? firstLine.substring(0, 12) + '...' : firstLine || 'Untitled';

      return {
        id: atom.id,
        depth: atom.depth,
        label,
        x: atom.depth === 0 ? centerX : centerX + (Math.random() - 0.5) * 80,
        y: atom.depth === 0 ? centerY : centerY + (Math.random() - 0.5) * 60,
        fx: atom.depth === 0 ? centerX : undefined,
        fy: atom.depth === 0 ? centerY : undefined,
      };
    });

    // Build links from edges that connect our limited atoms
    const nodeIds = new Set(limitedAtoms.map(a => a.id));
    const links = graph.edges
      .filter(e => nodeIds.has(e.source_id) && nodeIds.has(e.target_id))
      .map((edge) => ({
        source: edge.source_id,
        target: edge.target_id,
        strength: edge.strength,
      }));

    // Create simulation
    const simulation = d3.forceSimulation(initialNodes)
      .force('charge', d3.forceManyBody().strength(-120))
      .force('collide', d3.forceCollide().radius(35))
      .force('link', d3.forceLink(links)
        .id((d: any) => d.id)
        .distance(70)
        .strength((link: any) => link.strength * 0.3))
      .force('radial', d3.forceRadial(
        (d: SimulationNode) => d.depth === 0 ? 0 : 70,
        centerX,
        centerY
      ).strength(0.5))
      .alphaDecay(0.1)
      .velocityDecay(0.5);

    // Update nodes on each tick
    simulation.on('tick', () => {
      setNodes([...initialNodes]);
    });

    return () => {
      simulation.stop();
    };
  }, [graph]);

  const handleExpand = useCallback((e: React.MouseEvent) => {
    const newTab = e.metaKey || e.ctrlKey;
    if (onExpand) {
      onExpand({ newTab });
    } else {
      openLocalGraph(atomId, undefined, { newTab });
    }
  }, [atomId, onExpand, openLocalGraph]);

  // Calculate edges for rendering
  const edges = useMemo(() => {
    if (!graph) return [];
    const nodeMap = new Map(nodes.map(n => [n.id, n]));
    return graph.edges
      .map(edge => {
        const source = nodeMap.get(edge.source_id);
        const target = nodeMap.get(edge.target_id);
        if (!source || !target) return null;
        return { ...edge, source, target };
      })
      .filter((e): e is NonNullable<typeof e> => e !== null);
  }, [graph, nodes]);

  if (isLoading) {
    return (
      <div className="h-[120px] flex items-center justify-center text-sm text-[var(--color-text-tertiary)] bg-[var(--color-bg-panel)] rounded-md">
        Loading graph...
      </div>
    );
  }

  if (!graph || graph.atoms.length <= 1) {
    return (
      <div className="h-[80px] flex items-center justify-center text-sm text-[var(--color-text-tertiary)] bg-[var(--color-bg-panel)] rounded-md">
        No connections found
      </div>
    );
  }

  return (
    <div>
      <div
        className="relative bg-[var(--color-bg-main)] rounded-md overflow-hidden cursor-pointer ring-1 ring-transparent hover:ring-white/70 transition-shadow aspect-square w-3/4 mx-auto"
        onClick={handleExpand}
      >
        <svg width="100%" height="100%" viewBox="0 0 200 200" preserveAspectRatio="xMidYMid meet">
          {/* Edges */}
          {edges.map((edge) => (
            <line
              key={`${edge.source_id}-${edge.target_id}`}
              x1={edge.source.x}
              y1={edge.source.y}
              x2={edge.target.x}
              y2={edge.target.y}
              stroke={edge.edge_type === 'semantic' ? 'var(--color-accent)' : 'var(--color-text-tertiary)'}
              strokeWidth={1}
              strokeOpacity={0.4}
              strokeDasharray={edge.edge_type === 'semantic' ? '4,2' : undefined}
            />
          ))}

          {/* Nodes */}
          {nodes.map((node) => {
            const isCenter = node.depth === 0;
            return (
              <g key={node.id} transform={`translate(${node.x}, ${node.y})`}>
                <circle
                  r={isCenter ? 8 : 6}
                  fill={isCenter ? 'var(--color-accent)' : 'var(--color-bg-hover)'}
                  stroke={isCenter ? 'var(--color-accent-light)' : 'var(--color-border-hover)'}
                  strokeWidth={1}
                />
                <text
                  y={isCenter ? 18 : 14}
                  textAnchor="middle"
                  fill="var(--color-text-secondary)"
                  fontSize={8}
                  className="pointer-events-none"
                >
                  {node.label}
                </text>
              </g>
            );
          })}
        </svg>

        {/* Hint overlay */}
        <div className="absolute bottom-1 right-2 text-[10px] text-[var(--color-text-tertiary)]">
          Click to expand
        </div>
      </div>
    </div>
  );
}
