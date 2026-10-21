// Dati di prova per i test d'integrazione MongoDB (shell `mongo` o `mongosh`).
db = db.getSiblingDB("testdb");
db.dropDatabase();

const cities = ["Roma", "Milano", "Torino", null, "Napoli"];
const customers = [];
for (let n = 1; n <= 1234; n++) {
  const c = {
    _id: n,
    name: "Cliente " + n,
    email: "c" + n + "@example.com",
    address: { city: cities[n % 5], zip: String(10000 + n) },
    tags: n % 3 === 0 ? ["vip", "b2b"] : [],
    created: new Date(Date.UTC(2025, 0, 1) + n * 3600000),
  };
  if (n % 7 === 0) c.notes = "nota multiriga\nseconda riga";
  customers.push(c);
}
db.customers.insertMany(customers);
db.customers.createIndex({ email: 1 }, { unique: true });

const statuses = ["new", "paid", "shipped"];
let batch = [];
for (let n = 1; n <= 12345; n++) {
  batch.push({
    seq: n,
    customer_id: 1 + (n % 1234),
    total: Math.round(((n * 1.37) % 500) * 100) / 100,
    status: statuses[n % 3],
    ordered_on: new Date(Date.UTC(2025, 0, 1) + (n % 365) * 86400000),
    items: [
      { sku: "SKU" + String(n % 100).padStart(5, "0"), qty: 1 + (n % 4) },
      { sku: "SKU" + String((n + 7) % 100).padStart(5, "0"), qty: 1 },
    ],
  });
  if (batch.length === 1000) {
    db.orders.insertMany(batch);
    batch = [];
  }
}
db.orders.insertMany(batch);
db.orders.createIndex({ status: 1, ordered_on: -1 });
db.orders.createIndex({ customer_id: 1 });

db.createView("paid_orders", "orders", [{ $match: { status: "paid" } }]);
db.getCollection("event-log").insertMany([
  { kind: "login", at: new Date() },
  { kind: "logout", at: new Date(), extra: { ip: "10.0.0.1" } },
]);
