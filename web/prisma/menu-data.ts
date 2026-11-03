/**
 * The placeholder menu from the static site, shared by the Prisma seed and the engine seed.
 * Replace with the restaurant's real menu and prices (via the admin dashboard once built).
 * Prices are in minor units (1499 = 14.99).
 */

export const SIZES = [
  { name: "Regular", priceDelta: 0 },
  { name: "Large", priceDelta: 300 },
  { name: "X-Large", priceDelta: 500 },
];

export const EXTRAS = [
  { name: "Extra Cheese", price: 200 },
  { name: "Bacon", price: 250 },
  { name: "Avocado", price: 150 },
  { name: "Jalapeños", price: 100 },
];

export const categories = [
  { slug: "burgers", name: "Burgers", sortOrder: 1 },
  { slug: "pizza", name: "Pizza", sortOrder: 2 },
  { slug: "chicken", name: "Chicken", sortOrder: 3 },
  { slug: "wraps", name: "Wraps", sortOrder: 4 },
  { slug: "desserts", name: "Desserts", sortOrder: 5 },
  { slug: "pasta", name: "Pasta", sortOrder: 6 },
];

export const products = [
  { slug: "classic-smash-burger", name: "Classic Smash Burger", category: "burgers", basePrice: 1499, promoPrice: null, calories: 620, prepMinutes: 12, featured: true, station: "GRILL", imageUrl: "/img/menu/1.jpg", description: "Double smashed patty, cheddar, caramelized onions, pickles and special sauce on a toasted brioche bun.", allergens: ["gluten", "dairy", "egg"] },
  { slug: "margherita-royale", name: "Margherita Royale", category: "pizza", basePrice: 1999, promoPrice: null, calories: 480, prepMinutes: 18, featured: false, station: "GRILL", imageUrl: "/img/menu/2.jpg", description: "San Marzano tomatoes, buffalo mozzarella, basil and truffle oil on a hand-stretched sourdough base.", allergens: ["gluten", "dairy"] },
  { slug: "nashville-hot-chicken", name: "Nashville Hot Chicken", category: "chicken", basePrice: 1299, promoPrice: null, calories: 710, prepMinutes: 15, featured: true, station: "FRY", imageUrl: "/img/menu/3.jpg", description: "Extra-crispy fried chicken in our fiery Nashville spice blend with honey drizzle and pickles.", allergens: ["gluten", "egg"] },
  { slug: "loaded-fajita-wrap", name: "Loaded Fajita Wrap", category: "wraps", basePrice: 1099, promoPrice: null, calories: 520, prepMinutes: 10, featured: false, station: "GRILL", imageUrl: "/img/menu/4.jpg", description: "Grilled chicken, peppers, sour cream and guacamole in a warm flour tortilla with melted cheddar.", allergens: ["gluten", "dairy"] },
  { slug: "nutella-lava-cake", name: "Nutella Lava Cake", category: "desserts", basePrice: 899, promoPrice: null, calories: 390, prepMinutes: 8, featured: false, station: "PACKING", imageUrl: "/img/menu/5.jpg", description: "Warm molten chocolate cake with a Nutella centre, vanilla ice cream and salted caramel.", allergens: ["gluten", "dairy", "egg", "nuts"] },
  { slug: "truffle-mushroom-pasta", name: "Truffle Mushroom Pasta", category: "pasta", basePrice: 1699, promoPrice: null, calories: 560, prepMinutes: 20, featured: false, station: "GRILL", imageUrl: "/img/menu/6.jpg", description: "Tagliatelle with wild mushrooms, shaved black truffle, parmesan and thyme in garlic butter.", allergens: ["gluten", "dairy"] },
];

/** Desserts come in one size; everything else offers SIZES. */
export const hasSizes = (category: string) => category !== "desserts";

export const promos = [
  { code: "SAVE10", type: "PERCENT" as const, value: 10, minSubtotal: 0 },
  { code: "BURGER20", type: "PERCENT" as const, value: 20, minSubtotal: 0 },
  { code: "WELCOME", type: "FIXED" as const, value: 500, minSubtotal: 0 },
];
