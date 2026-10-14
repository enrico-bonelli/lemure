DROP TABLE IF EXISTS order_items, orders, products, customers;
CREATE TABLE customers (
  id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(100) NOT NULL,
  email VARCHAR(150) NOT NULL,
  city VARCHAR(80) NULL,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  notes TEXT,
  UNIQUE KEY uq_customers_email (email),
  KEY idx_customers_city (city)
) ENGINE=InnoDB;
CREATE TABLE products (
  sku CHAR(8) PRIMARY KEY,
  title VARCHAR(200) NOT NULL,
  price DECIMAL(10,2) NOT NULL,
  data JSON NULL,
  thumb BLOB NULL
) ENGINE=InnoDB;
CREATE TABLE orders (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  customer_id INT UNSIGNED NOT NULL,
  total DECIMAL(12,2) NOT NULL,
  status ENUM('new','paid','shipped') NOT NULL DEFAULT 'new',
  ordered_on DATE NOT NULL,
  CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE ON UPDATE RESTRICT,
  KEY idx_orders_status_date (status, ordered_on)
) ENGINE=InnoDB;
CREATE TABLE order_items (
  order_id BIGINT NOT NULL,
  sku CHAR(8) NOT NULL,
  qty INT NOT NULL,
  PRIMARY KEY (order_id, sku),
  CONSTRAINT fk_items_order FOREIGN KEY (order_id) REFERENCES orders(id),
  CONSTRAINT fk_items_product FOREIGN KEY (sku) REFERENCES products(sku)
) ENGINE=InnoDB;
CREATE OR REPLACE VIEW v_customer_totals AS SELECT c.id, c.name, SUM(o.total) tot FROM customers c JOIN orders o ON o.customer_id=c.id GROUP BY c.id, c.name;
SET SESSION cte_max_recursion_depth = 100000;
INSERT INTO customers (name, email, city, notes)
WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 12345)
SELECT CONCAT('Cliente ', n), CONCAT('c', n, '@example.com'), ELT(1 + n % 5, 'Roma','Milano','Torino',NULL,'Napoli'),
       IF(n % 7 = 0, CONCAT('nota multiriga\nseconda riga; con "virgolette" e ; separatore ', n), NULL) FROM seq;
INSERT INTO products VALUES ('SKU00001','Penna',1.50,'{"color":"blu"}',X'89504E47'),('SKU00002','Quaderno',3.20,NULL,NULL),('SKU00003','Zaino',29.90,'{"size":[1,2]}',NULL);
INSERT INTO orders (customer_id,total,status,ordered_on)
WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 30000)
SELECT 1 + n % 12345, ROUND(n * 1.37 % 500, 2), ELT(1 + n % 3,'new','paid','shipped'), DATE('2025-01-01') + INTERVAL (n % 365) DAY FROM seq;
INSERT INTO order_items SELECT id, 'SKU00001', 1 FROM orders WHERE id <= 1000;
