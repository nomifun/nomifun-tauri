export function isRequirementSearchExpanded(active: boolean, query: string): boolean {
  return active || query.length > 0;
}

export function shouldCollapseRequirementSearch(query: string): boolean {
  return query.trim().length === 0;
}
