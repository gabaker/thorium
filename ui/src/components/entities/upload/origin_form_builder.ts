import { OriginState } from './reducer';
import { OriginType } from './types';

/**
 * Validates the origin state and appends the appropriate fields to the given FormData.
 * Returns null on success, or a validation error message string on failure.
 */
export function appendOriginToForm(formData: FormData, origin: OriginState): string | null {
  const appendField = (field: string, value: string | number) => formData.append(`origin[${field}]`, String(value));

  const appendFieldList = (field: string, values: string[]) => values.forEach((v) => formData.append(`origin[${field}]`, v));

  // Downloaded origin: requires URL when name is set
  if (origin.origin_type === OriginType.Downloaded) {
    if (origin.url) {
      appendField('origin_type', origin.origin_type);
      appendField('url', origin.url);
      if (origin.name) appendField('name', origin.name);
    } else if (origin.name) {
      return 'ORIGIN field "SITE NAME" set while necessary field "URL" is blank';
    }

    // Transformed / Unpacked origin: requires parent when tool or flags are set
  } else if (origin.origin_type === OriginType.Transformed || origin.origin_type === OriginType.Unpacked) {
    if (origin.parent) {
      appendField('origin_type', origin.origin_type);
      appendField('parent', origin.parent);
      if (origin.tool) appendField('tool', origin.tool);
      if (origin.flags.length > 0) appendFieldList('flags', origin.flags);
      if (origin.cmd) appendField('cmd', origin.cmd);
    } else if (origin.tool || origin.flags.length > 0) {
      return 'ORIGIN field set while necessary field "PARENT" is blank';
    }

    // Carved origin: requires parent and carved_type; Pcap sub-type has additional network fields
  } else if (origin.origin_type === OriginType.Carved) {
    if (origin.parent) {
      if (!origin.carved_type) {
        return 'ORIGIN "Carved" needs a specified type';
      }
      appendField('parent', origin.parent);
      if (origin.tool) appendField('tool', origin.tool);
      const fullOriginType = origin.origin_type + origin.carved_type;
      appendField('origin_type', fullOriginType);

      if (fullOriginType === 'CarvedPcap') {
        if (origin.src_ip) appendField('src_ip', origin.src_ip);
        if (origin.dest_ip) appendField('dest_ip', origin.dest_ip);
        if (origin.src_port) appendField('src_port', origin.src_port);
        if (origin.dest_port) appendField('dest_port', origin.dest_port);
        if (origin.proto) appendField('proto', origin.proto);
        if (origin.pcap_url) appendField('url', origin.pcap_url);
      }
    } else if (origin.tool) {
      return 'ORIGIN field "TOOL" set while necessary field "PARENT" is blank';
    }

    // Wire origin: requires sniffer when source or destination are set
  } else if (origin.origin_type === OriginType.Wire) {
    if (origin.sniffer) {
      appendField('origin_type', origin.origin_type);
      appendField('sniffer', origin.sniffer);
      if (origin.source) appendField('source', origin.source);
      if (origin.destination) appendField('destination', origin.destination);
    } else if (origin.source || origin.destination) {
      return 'ORIGIN field set while necessary field "SNIFFER" is blank';
    }

    // Incident origin: requires incident ID when other incident fields are set
  } else if (origin.origin_type === OriginType.Incident) {
    if (origin.incident) {
      appendField('origin_type', origin.origin_type);
      appendField('incident', origin.incident);
      if (origin.mission_team) appendField('mission_team', origin.mission_team);
      if (origin.cover_term) appendField('cover_term', origin.cover_term);
      if (origin.network) appendField('network', origin.network);
      if (origin.machine) appendField('machine', origin.machine);
      if (origin.location) appendField('location', origin.location);
    } else if (origin.cover_term || origin.mission_team || origin.network || origin.machine || origin.location) {
      return 'ORIGIN field set while necessary field "INCIDENT ID" is blank';
    }

    // MemoryDump origin: requires memory_type when other memory fields are set
  } else if (origin.origin_type === OriginType.MemoryDump) {
    if (origin.memory_type) {
      appendField('origin_type', origin.origin_type);
      appendField('memory_type', origin.memory_type);
      if (origin.parent) appendField('parent', origin.parent);
      if (origin.reconstructed.length > 0) appendFieldList('reconstructed', origin.reconstructed);
      if (origin.base_addr) appendField('base_addr', origin.base_addr);
    } else if (origin.parent || origin.reconstructed.length > 0 || origin.base_addr) {
      return 'ORIGIN field set while necessary field "MEMORY TYPE" is blank';
    }

    // Source origin: requires repo when other source fields are set
  } else if (origin.origin_type === OriginType.Source) {
    if (origin.repo) {
      appendField('origin_type', origin.origin_type);
      appendField('repo', origin.repo);
      if (origin.commitish) appendField('commitish', origin.commitish);
      if (origin.commit) appendField('commit', origin.commit);
      if (origin.system) appendField('system', origin.system);
      if (origin.flags.length > 0) appendFieldList('flags', origin.flags);
      if (origin.supporting !== undefined) appendField('supporting', String(origin.supporting));
    } else if (origin.commit || origin.system || origin.commitish || origin.flags.length > 0) {
      return 'ORIGIN field set while necessary field "REPOSITORY" is blank';
    }
  }

  // Append result IDs shared across all origin types
  if (origin.result_ids.length > 0) {
    appendFieldList('result_ids', origin.result_ids);
  }

  return null;
}
