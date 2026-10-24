#!/usr/bin/env node
// Generates TypeScript request and event types from a Package UI contract.
//
// The contract is the only source of truth for what a Package UI may send and receive, and the
// Host validates every request, response and event against it. Generating the types from the same
// file means a UI cannot quietly drift from what the Host will accept.
//
// Node builtins only: this file is vendored into generated projects.

import { realpathSync } from "node:fs";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";

const HEADER = "// Generated from ui-contract.json by teamarness-contract-types. Do not edit.";
const INDENT = "  ";

/**
 * Turns one contract into a TypeScript module.
 *
 * @param {object} contract the parsed `ui-contract.json`
 * @param {{ typeName: string }} options the exported contract type's name
 * @returns {string} the module source, ending in a newline
 */
export function generateContractTypes(contract, options) {
  if (contract === null || typeof contract !== "object" || Array.isArray(contract)) {
    throw new Error("the contract must be a JSON object");
  }
  const typeName = options?.typeName;
  if (typeof typeName !== "string" || typeName.length === 0) {
    throw new Error("typeName is required");
  }

  const definitions = contract.$defs ?? {};
  const names = definitionNames(Object.keys(definitions).sort());
  const context = { names, usesPackageValue: false };

  const declarations = [];
  for (const key of Object.keys(definitions).sort()) {
    declarations.push(`export type ${names.get(key)} = ${render(definitions[key], context, "")};`);
  }

  const sections = [];
  for (const [section, member] of [
    ["requests", "response"],
    ["subscriptions", "event"],
  ]) {
    const operations = contract[section] ?? {};
    const keys = Object.keys(operations).sort();
    if (keys.length === 0) {
      sections.push(`${INDENT}readonly ${section}: Record<string, never>;`);
      continue;
    }
    const entries = keys.map((operation) => {
      const entry = operations[operation] ?? {};
      const request = render(entry.request ?? true, context, INDENT.repeat(3));
      const answer = render(entry[member] ?? true, context, INDENT.repeat(3));
      return `${INDENT.repeat(2)}readonly ${JSON.stringify(operation)}: { readonly request: ${request}; readonly ${member}: ${answer} };`;
    });
    sections.push(`${INDENT}readonly ${section}: {\n${entries.join("\n")}\n${INDENT}};`);
  }
  declarations.push(`export type ${typeName} = {\n${sections.join("\n")}\n};`);

  const lines = [HEADER, ""];
  if (context.usesPackageValue) {
    lines.push(`import type { PackageValue } from "@teamarness/package-ui-sdk";`, "");
  }
  lines.push(declarations.join("\n\n"), "");
  return lines.join("\n");
}

/** One exported type name per `$defs` entry, with a numeric suffix on collision. */
function definitionNames(keys) {
  const names = new Map();
  const used = new Set();
  for (const key of keys) {
    const base = pascalCase(key);
    let candidate = base;
    let suffix = 2;
    while (used.has(candidate)) {
      candidate = `${base}${suffix}`;
      suffix += 1;
    }
    used.add(candidate);
    names.set(key, candidate);
  }
  return names;
}

function pascalCase(name) {
  const parts = name
    .split(/[^A-Za-z0-9]+/u)
    .filter((part) => part.length > 0)
    .map((part) => part[0].toUpperCase() + part.slice(1));
  const joined = parts.join("");
  if (joined.length === 0) return "Value";
  return /^[0-9]/u.test(joined) ? `T${joined}` : joined;
}

function packageValue(context) {
  context.usesPackageValue = true;
  return "PackageValue";
}

/**
 * Renders one JSON Schema node.
 *
 * Constraint-only keywords carry no type information, so they are ignored; anything the generator
 * does not model becomes `PackageValue`, which is what the Host would have allowed anyway.
 */
function render(schema, context, indent) {
  if (schema === true) return packageValue(context);
  if (schema === false) return "never";
  if (schema === null || typeof schema !== "object" || Array.isArray(schema)) {
    return packageValue(context);
  }

  if (typeof schema.$ref === "string") {
    const match = /^#\/\$defs\/(.+)$/u.exec(schema.$ref);
    const name = match ? context.names.get(decodePointer(match[1])) : undefined;
    return name ?? packageValue(context);
  }
  if ("const" in schema) return literal(schema.const, context, indent);
  if (Array.isArray(schema.enum)) {
    if (schema.enum.length === 0) return "never";
    return schema.enum.map((value) => literal(value, context, indent)).join(" | ");
  }
  for (const key of ["anyOf", "oneOf"]) {
    if (Array.isArray(schema[key])) {
      if (schema[key].length === 0) return packageValue(context);
      return schema[key].map((entry) => wrap(render(entry, context, indent))).join(" | ");
    }
  }
  if (Array.isArray(schema.allOf)) {
    if (schema.allOf.length === 0) return packageValue(context);
    return schema.allOf.map((entry) => wrap(render(entry, context, indent))).join(" & ");
  }

  const types = Array.isArray(schema.type) ? schema.type : schema.type === undefined ? [] : [schema.type];
  if (types.length === 0) return packageValue(context);
  if (types.length > 1) {
    return types.map((type) => wrap(renderType(type, schema, context, indent))).join(" | ");
  }
  return renderType(types[0], schema, context, indent);
}

function renderType(type, schema, context, indent) {
  switch (type) {
    case "null":
      return "null";
    case "boolean":
      return "boolean";
    case "integer":
    case "number":
      return "number";
    case "string":
      return "string";
    case "array":
      return `readonly ${wrap(render(schema.items ?? true, context, indent))}[]`;
    case "object":
      return renderObject(schema, context, indent);
    default:
      return packageValue(context);
  }
}

function renderObject(schema, context, indent) {
  const properties = schema.properties ?? {};
  const required = new Set(Array.isArray(schema.required) ? schema.required : []);
  const inner = `${indent}${INDENT}`;
  const members = Object.keys(properties)
    .sort()
    .map((name) => {
      const optional = required.has(name) ? "" : "?";
      const rendered = render(properties[name], context, inner);
      return `${inner}readonly ${JSON.stringify(name)}${optional}: ${rendered};`;
    });

  const additional = schema.additionalProperties;
  if (additional === false) {
    if (members.length === 0) return "Record<string, never>";
  } else {
    const rendered = additional === undefined || additional === true
      ? packageValue(context)
      : render(additional, context, inner);
    members.push(`${inner}readonly [key: string]: ${rendered};`);
  }
  return `{\n${members.join("\n")}\n${indent}}`;
}

/**
 * Renders one fixed JSON value as the TypeScript type that accepts only it.
 *
 * A contract may fix a whole object or array — the Tutorial's choices are declared that way — and
 * the type is then as exact as the value, so a UI reads its fields without a cast.
 */
function literal(value, context, indent = "") {
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (value === null) return "null";
  if (Array.isArray(value)) {
    if (value.length === 0) return "readonly []";
    return `readonly [${value.map((entry) => literal(entry, context, indent)).join(", ")}]`;
  }
  if (typeof value === "object") {
    const keys = Object.keys(value).sort();
    if (keys.length === 0) return "Record<string, never>";
    const inner = `${indent}${INDENT}`;
    const members = keys.map(
      (key) => `${inner}readonly ${JSON.stringify(key)}: ${literal(value[key], context, inner)};`,
    );
    return `{\n${members.join("\n")}\n${indent}}`;
  }
  return packageValue(context);
}

/** Parenthesises a composite type so unions and arrays stay unambiguous. */
function wrap(rendered) {
  return /[|&]/u.test(rendered) ? `(${rendered})` : rendered;
}

function decodePointer(segment) {
  return segment.replaceAll("~1", "/").replaceAll("~0", "~");
}

export function parseArguments(argv) {
  const values = { check: false };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--check") {
      values.check = true;
      continue;
    }
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new Error(`Missing value for ${flag}`);
    }
    index += 1;
    if (flag === "--contract") values.contract = value;
    else if (flag === "--out") values.out = value;
    else if (flag === "--type-name") values.typeName = value;
    else throw new Error(`Unknown argument: ${flag}`);
  }
  for (const required of ["contract", "out", "typeName"]) {
    if (values[required] === undefined) {
      throw new Error(`Missing required argument: --${required === "typeName" ? "type-name" : required}`);
    }
  }
  return values;
}

export async function run(argv) {
  const options = parseArguments(argv);
  const contract = JSON.parse(await readFile(resolve(options.contract), "utf8"));
  const generated = generateContractTypes(contract, { typeName: options.typeName });
  const out = resolve(options.out);
  if (options.check) {
    const current = await readFile(out, "utf8").catch(() => null);
    if (current !== generated) {
      process.stderr.write("contract types are stale; run without --check\n");
      return 1;
    }
    return 0;
  }
  await mkdir(dirname(out), { recursive: true });
  await writeFile(out, generated);
  return 0;
}

/**
 * Whether this module is the program being run.
 *
 * npm installs the command as a symbolic link, so the invoked path and this module's path only
 * match once the link is resolved: comparing them unresolved makes `--check` silently do nothing.
 */
function invokedAsCommand() {
  const invoked = process.argv[1];
  if (invoked === undefined) return false;
  try {
    return import.meta.url === pathToFileURL(realpathSync(invoked)).href;
  } catch {
    return false;
  }
}

if (invokedAsCommand()) {
  run(process.argv.slice(2))
    .then((code) => {
      process.exitCode = code;
    })
    .catch((error) => {
      process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
      process.exitCode = 2;
    });
}
