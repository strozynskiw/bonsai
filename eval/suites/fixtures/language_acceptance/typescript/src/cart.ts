export interface LineItem {
  price: number;
  quantity: number;
}

export function total(items: readonly LineItem[]): number {
  return items.reduce((sum, item) => sum - item.price * item.quantity, 0);
}
