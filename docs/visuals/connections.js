// Route diagram connectors from the rendered cards, after fonts have loaded.
export function drawConnections(board, connections) {
  const svg = board.querySelector('.arrows');
  const origin = board.getBoundingClientRect();
  svg.setAttribute('viewBox', `0 0 ${origin.width} ${origin.height}`);
  const box = (selector) => {
    const element = board.querySelector(selector);
    if (!element) throw new Error(`Missing diagram node: ${selector}`);
    const rect = element.getBoundingClientRect();
    return {
      left: rect.left - origin.left,
      right: rect.right - origin.left,
      top: rect.top - origin.top,
      bottom: rect.bottom - origin.top,
      width: rect.width,
      height: rect.height,
    };
  };
  const port = (rect, side, offset, gap) => {
    switch (side) {
      case 'left':
        return [rect.left - gap, rect.top + rect.height * offset];
      case 'right':
        return [rect.right + gap, rect.top + rect.height * offset];
      case 'top':
        return [rect.left + rect.width * offset, rect.top - gap];
      case 'bottom':
        return [rect.left + rect.width * offset, rect.bottom + gap];
      default:
        throw new Error(`Unknown connector side: ${side}`);
    }
  };
  for (const edge of connections) {
    const from = box(edge.from);
    const to = box(edge.to);
    const fromSide = edge.fromSide ?? 'right';
    const toSide = edge.toSide ?? 'left';
    const horizontal = ['left', 'right'].includes(fromSide);
    if (horizontal !== ['left', 'right'].includes(toSide)) {
      throw new Error('Connector ports must use the same axis');
    }
    const start = port(from, fromSide, edge.fromOffset ?? 0.5, 6);
    const end = port(to, toSide, edge.toOffset ?? 0.5, 10);
    if (edge.align) {
      const low = Math.max(horizontal ? from.top : from.left, horizontal ? to.top : to.left);
      const high = Math.min(
        horizontal ? from.bottom : from.right,
        horizontal ? to.bottom : to.right,
      );
      if (high <= low) throw new Error('Aligned connector requires overlapping nodes');
      start[horizontal ? 1 : 0] = end[horizontal ? 1 : 0] = (low + high) / 2;
    }
    const middle = horizontal ? (start[0] + end[0]) / 2 : (start[1] + end[1]) / 2;
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute(
      'd',
      horizontal
        ? `M${start[0]} ${start[1]} H${middle} V${end[1]} H${end[0]}`
        : `M${start[0]} ${start[1]} V${middle} H${end[0]} V${end[1]}`,
    );
    path.setAttribute('marker-end', `url(#${edge.secondary ? 'secondary-arrow' : 'arrow'})`);
    if (edge.secondary) path.classList.add('secondary');
    svg.append(path);
  }
}
