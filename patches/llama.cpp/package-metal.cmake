# Included only by package-macos.sh, after llama.cpp's top-level project().
# Keep upstream's embedded sources and patch only a build-directory copy.
function(private_translate_package_metal)
    get_target_property(source_dir ggml-metal SOURCE_DIR)
    get_target_property(sources ggml-metal SOURCES)
    if(NOT "ggml-metal-device.m" IN_LIST sources OR NOT GGML_METAL_EMBED_LIBRARY)
        message(FATAL_ERROR "Expected pinned ggml-metal sources with embedded shaders enabled")
    endif()
    set(device_source "${CMAKE_CURRENT_BINARY_DIR}/ggml-metal-device.m")
    execute_process(
        COMMAND /usr/bin/patch -fs -F 0 -o "${device_source}"
                "${source_dir}/ggml-metal-device.m" "${CMAKE_CURRENT_FUNCTION_LIST_DIR}/bundled-metal.patch"
        COMMAND_ERROR_IS_FATAL ANY
    )
    list(TRANSFORM sources REPLACE "^ggml-metal-device\\.m$" "${device_source}")
    set_property(TARGET ggml-metal PROPERTY SOURCES "${sources}")
    target_include_directories(ggml-metal PRIVATE "${source_dir}")

    # Read the kernel list from the same target's generated assembly. The two
    # shared Flash Attention .metal includes are headers, not kernel inputs.
    list(FILTER sources INCLUDE REGEX "/ggml-metal-embed-.*\\.s$")
    if(NOT sources)
        message(FATAL_ERROR "Could not find the pinned embedded Metal kernel list")
    endif()
    file(GLOB headers CONFIGURE_DEPENDS
        "${source_dir}/kernels/*.h" "${source_dir}/kernels/fa*common.metal"
        "${source_dir}/*.h" "${source_dir}/../ggml-common.h"
    )
    set(output "${GGML_PRIVATE_TRANSLATE_METALLIB_DIR}")
    file(MAKE_DIRECTORY "${output}")
    set(metal_xcrun /usr/bin/xcrun)
    if(NOT "$ENV{PRIVATE_TRANSLATE_METAL_DEVELOPER_DIR}" STREQUAL "")
        set(metal_xcrun "${CMAKE_COMMAND}" -E env
            "DEVELOPER_DIR=$ENV{PRIVATE_TRANSLATE_METAL_DEVELOPER_DIR}" /usr/bin/xcrun)
    endif()
    set(base_air "")
    foreach(source IN LISTS sources)
        get_filename_component(stem "${source}" NAME_WE)
        string(REGEX REPLACE "^ggml-metal-embed-" "" kernel "${stem}")
        set(input "${source_dir}/kernels/${kernel}.metal")
        set(air "${CMAKE_CURRENT_BINARY_DIR}/${kernel}.air")
        private_translate_metal_air("${input}" "${air}" 3.0 "${CMAKE_OSX_DEPLOYMENT_TARGET}")
        list(APPEND base_air "${air}")
    endforeach()

    # Upstream uses these two sources for the separate Tensor API library.
    set(tensor_air "")
    foreach(kernel IN ITEMS mul_mm fa_tensor)
        set(air "${CMAKE_CURRENT_BINARY_DIR}/${kernel}-tensor.air")
        private_translate_metal_air("${source_dir}/kernels/${kernel}.metal" "${air}"
            4.0 26.0 -DGGML_METAL_HAS_TENSOR)
        list(APPEND tensor_air "${air}")
    endforeach()

    private_translate_link_metallib("${output}/default.metallib" ${base_air})
    private_translate_link_metallib("${output}/ggml-tensor.metallib" ${tensor_air})
    add_custom_target(private-translate-metallibs ALL
        DEPENDS "${output}/default.metallib" "${output}/ggml-tensor.metallib")
endfunction()

function(private_translate_metal_air input output language deployment)
    add_custom_command(OUTPUT "${output}"
        COMMAND ${metal_xcrun} -sdk macosx metal -O3 "-std=metal${language}"
                "-mmacosx-version-min=${deployment}" -I "${source_dir}" -I "${source_dir}/.."
                ${ARGN} -c "${input}" -o "${output}"
        DEPENDS "${input}" ${headers}
        COMMENT "Compiling Metal IR for ${input}"
        VERBATIM)
endfunction()

function(private_translate_link_metallib output)
    add_custom_command(OUTPUT "${output}"
        COMMAND ${metal_xcrun} -sdk macosx metallib ${ARGN} -o "${output}"
        DEPENDS ${ARGN}
        VERBATIM)
endfunction()

if(CMAKE_CURRENT_SOURCE_DIR STREQUAL CMAKE_SOURCE_DIR AND CMAKE_SYSTEM_NAME STREQUAL "Darwin")
    cmake_language(DEFER CALL private_translate_package_metal)
endif()
