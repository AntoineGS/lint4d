unit T;

interface

function GetContent: {$IFDEF WBB_ANSI}AnsiString{$ELSE}string{$ENDIF};

implementation

function GetContent: {$IFDEF WBB_ANSI}AnsiString{$ELSE}string{$ENDIF};
begin
  Result := '';
end;

end.
